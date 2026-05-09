//! Channel manager handling of streaming metadata (`_stream_delta`, `_stream_end`) and send retries.

use std::any::Any;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use serde_json::{Value, json};
use serial_test::serial;
use xbot::channels::{
    Channel, ChannelBase, ChannelDescriptor, ChannelManager, clear_plugins, register_plugin,
};
use xbot::config::ChannelsConfig;
use xbot::storage::{MessageBus, OutboundMessage};

struct StreamProbeChannel {
    base: ChannelBase,
    send_contents: Arc<Mutex<Vec<String>>>,
    delta_contents: Arc<Mutex<Vec<String>>>,
    fail_send_remaining: Arc<std::sync::atomic::AtomicUsize>,
}

impl StreamProbeChannel {
    fn new(
        base: ChannelBase,
        send_contents: Arc<Mutex<Vec<String>>>,
        delta_contents: Arc<Mutex<Vec<String>>>,
        fail_send_remaining: Arc<std::sync::atomic::AtomicUsize>,
    ) -> Self {
        Self {
            base,
            send_contents,
            delta_contents,
            fail_send_remaining,
        }
    }
}

#[async_trait]
impl Channel for StreamProbeChannel {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn base(&self) -> &ChannelBase {
        &self.base
    }

    fn name(&self) -> &'static str {
        "streamprobe"
    }

    fn supports_streaming(&self) -> bool {
        true
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
        self.send_contents
            .lock()
            .expect("send lock")
            .push(msg.content.clone());
        let remaining = self
            .fail_send_remaining
            .load(std::sync::atomic::Ordering::SeqCst);
        if remaining > 0 {
            self.fail_send_remaining
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            return Err(anyhow!("simulated transport failure"));
        }
        Ok(())
    }

    async fn send_delta(
        &self,
        _chat_id: &str,
        delta: &str,
        _metadata: &BTreeMap<String, Value>,
    ) -> Result<()> {
        self.delta_contents
            .lock()
            .expect("delta lock")
            .push(delta.to_string());
        Ok(())
    }
}

fn register_stream_probe(
    send_contents: Arc<Mutex<Vec<String>>>,
    delta_contents: Arc<Mutex<Vec<String>>>,
    fail_send_remaining: Arc<std::sync::atomic::AtomicUsize>,
) {
    register_plugin(ChannelDescriptor::new(
        "streamprobe",
        "Stream Probe",
        json!({"enabled": false, "allowFrom": ["*"]}),
        Arc::new(move |config, bus, workspace, transcription_api_key| {
            Ok(Arc::new(StreamProbeChannel::new(
                ChannelBase::new(config, bus, workspace, transcription_api_key),
                send_contents.clone(),
                delta_contents.clone(),
                fail_send_remaining.clone(),
            )) as Arc<dyn Channel>)
        }),
    ));
}

#[tokio::test]
#[serial]
async fn stream_delta_goes_to_send_delta_not_send() {
    clear_plugins();
    let send_contents = Arc::new(Mutex::new(Vec::<String>::new()));
    let delta_contents = Arc::new(Mutex::new(Vec::<String>::new()));
    let fail_send = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    register_stream_probe(
        send_contents.clone(),
        delta_contents.clone(),
        fail_send.clone(),
    );

    let bus = MessageBus::new(16);
    let mut cfg: ChannelsConfig = serde_json::from_value(json!({
        "streamprobe": {"enabled": true, "allowFrom": ["*"]},
        "sendMaxRetries": 3
    }))
    .unwrap();
    cfg.send_max_retries = 3;

    let manager = ChannelManager::new(cfg, bus.clone(), PathBuf::new()).unwrap();
    manager.start_all().await.unwrap();

    let mut meta = BTreeMap::new();
    meta.insert("_stream_delta".to_string(), json!(true));
    bus.publish_outbound(OutboundMessage {
        channel: "streamprobe".to_string(),
        chat_id: "room".to_string(),
        content: "partial text".to_string(),
        reply_to: None,
        media: Vec::new(),
        reasoning_content: None,
        metadata: meta,
    })
    .await
    .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
    manager.stop_all().await.unwrap();

    assert_eq!(
        delta_contents.lock().unwrap().as_slice(),
        &["partial text".to_string()],
        "delta should be delivered via send_delta"
    );
    assert!(
        send_contents.lock().unwrap().is_empty(),
        "stream delta must not use regular send(); got {:?}",
        send_contents.lock().unwrap()
    );
    clear_plugins();
}

#[tokio::test]
#[serial]
async fn stream_end_without_streamed_skips_regular_send() {
    clear_plugins();
    let send_contents = Arc::new(Mutex::new(Vec::<String>::new()));
    let delta_contents = Arc::new(Mutex::new(Vec::<String>::new()));
    let fail_send = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    register_stream_probe(
        send_contents.clone(),
        delta_contents.clone(),
        fail_send.clone(),
    );

    let bus = MessageBus::new(16);
    let cfg: ChannelsConfig = serde_json::from_value(json!({
        "streamprobe": {"enabled": true, "allowFrom": ["*"]}
    }))
    .unwrap();
    let manager = ChannelManager::new(cfg, bus.clone(), PathBuf::new()).unwrap();
    manager.start_all().await.unwrap();

    let mut meta = BTreeMap::new();
    meta.insert("_stream_end".to_string(), json!(true));
    bus.publish_outbound(OutboundMessage {
        channel: "streamprobe".to_string(),
        chat_id: "room".to_string(),
        content: String::new(),
        reply_to: None,
        media: Vec::new(),
        reasoning_content: None,
        metadata: meta,
    })
    .await
    .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
    manager.stop_all().await.unwrap();

    assert!(
        send_contents.lock().unwrap().is_empty(),
        "stream end without _streamed must not invoke channel.send"
    );
    assert!(
        delta_contents.lock().unwrap().len() >= 1,
        "stream end should still invoke send_delta for the terminal segment"
    );
    clear_plugins();
}

#[tokio::test]
#[serial]
async fn send_retries_use_exponential_backoff_before_succeeding() {
    clear_plugins();
    let send_contents = Arc::new(Mutex::new(Vec::<String>::new()));
    let delta_contents = Arc::new(Mutex::new(Vec::<String>::new()));
    let fail_send = Arc::new(std::sync::atomic::AtomicUsize::new(2));

    register_stream_probe(
        send_contents.clone(),
        delta_contents.clone(),
        fail_send.clone(),
    );

    let bus = MessageBus::new(16);
    let mut cfg: ChannelsConfig = serde_json::from_value(json!({
        "streamprobe": {"enabled": true, "allowFrom": ["*"]}
    }))
    .unwrap();
    cfg.send_max_retries = 4;

    let manager = ChannelManager::new(cfg, bus.clone(), PathBuf::new()).unwrap();
    manager.start_all().await.unwrap();

    let mut meta = BTreeMap::new();
    meta.insert("_stream_end".to_string(), json!(true));
    meta.insert("_streamed".to_string(), json!(true));

    let start = Instant::now();
    bus.publish_outbound(OutboundMessage {
        channel: "streamprobe".to_string(),
        chat_id: "room".to_string(),
        content: "final".to_string(),
        reply_to: None,
        media: Vec::new(),
        reasoning_content: None,
        metadata: meta,
    })
    .await
    .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(3500)).await;
    manager.stop_all().await.unwrap();

    let sends = send_contents.lock().unwrap().clone();
    assert!(
        sends.len() >= 3,
        "expected at least 3 send attempts (2 failures + success), got {sends:?}"
    );
    assert_eq!(sends.last().map(|s| s.as_str()), Some("final"));

    let elapsed = start.elapsed();
    assert!(
        elapsed.as_secs() >= 2,
        "expected backoff delays (1s + 2s) before final success; elapsed {elapsed:?}"
    );
    assert_eq!(fail_send.load(std::sync::atomic::Ordering::SeqCst), 0);
    clear_plugins();
}
