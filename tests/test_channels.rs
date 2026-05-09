use std::any::Any;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use async_trait::async_trait;
use serde_json::json;
use serial_test::serial;
use xbot::channels::{
    Channel, ChannelBase, ChannelDescriptor, ChannelManager, LocalChannel, clear_plugins,
    discover_all, discover_channel_names, discover_plugins, register_plugin,
};
use xbot::config::ChannelsConfig;
use xbot::storage::{MessageBus, OutboundMessage};

struct DummyChannel {
    base: ChannelBase,
}

#[async_trait]
impl Channel for DummyChannel {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn base(&self) -> &ChannelBase {
        &self.base
    }

    fn name(&self) -> &'static str {
        "dummy"
    }

    async fn start(&self) -> Result<()> {
        self.base.set_running(true);
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.base.set_running(false);
        Ok(())
    }

    async fn send(&self, _msg: OutboundMessage) -> Result<()> {
        Ok(())
    }
}

#[test]
fn is_allowed_requires_exact_match() {
    let channel = DummyChannel {
        base: ChannelBase::new(
            json!({"allowFrom":["allow@email.com"]}),
            MessageBus::new(4),
            PathBuf::new(),
            String::new(),
        ),
    };
    assert!(channel.base().is_allowed("allow@email.com"));
    assert!(!channel.base().is_allowed("attacker|allow@email.com"));
}

#[test]
fn channels_config_accepts_unknown_keys() {
    let cfg: ChannelsConfig = serde_json::from_value(json!({
        "myplugin": {"enabled": true, "token": "abc"}
    }))
    .unwrap();
    assert_eq!(cfg.section("myplugin").unwrap()["enabled"], json!(true));
    assert_eq!(cfg.section("myplugin").unwrap()["token"], json!("abc"));
}

#[test]
#[serial]
fn discover_plugins_and_builtin_priority() {
    clear_plugins();
    register_plugin(ChannelDescriptor::new(
        "line",
        "Line",
        json!({"enabled": false}),
        Arc::new(|config, bus, workspace, transcription_api_key| {
            Ok(Arc::new(DummyChannel {
                base: ChannelBase::new(config, bus, workspace, transcription_api_key),
            }))
        }),
    ));
    register_plugin(ChannelDescriptor::new(
        "local",
        "Fake Local",
        json!({"enabled": false}),
        Arc::new(|config, bus, workspace, transcription_api_key| {
            Ok(Arc::new(DummyChannel {
                base: ChannelBase::new(config, bus, workspace, transcription_api_key),
            }))
        }),
    ));

    let plugins = discover_plugins();
    assert!(plugins.contains_key("line"));
    let all = discover_all();
    assert!(discover_channel_names().contains(&"local".to_string()));
    assert!(all.contains_key("line"));
    assert_eq!(all.get("local").unwrap().display_name, "Local");
    clear_plugins();
}

#[tokio::test]
#[serial]
async fn manager_loads_plugin_from_dict_config_and_dispatches() {
    clear_plugins();
    let sent = Arc::new(Mutex::new(Vec::<String>::new()));
    let sent_ref = sent.clone();
    register_plugin(ChannelDescriptor::new(
        "fakeplugin",
        "Fake Plugin",
        json!({"enabled": false}),
        Arc::new(move |config, bus, workspace, transcription_api_key| {
            let sent = sent_ref.clone();
            struct FakeChannel {
                base: ChannelBase,
                sent: Arc<Mutex<Vec<String>>>,
            }
            #[async_trait]
            impl Channel for FakeChannel {
                fn as_any(&self) -> &dyn Any {
                    self
                }
                fn base(&self) -> &ChannelBase {
                    &self.base
                }
                fn name(&self) -> &'static str {
                    "fakeplugin"
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
                    self.sent.lock().unwrap().push(msg.content);
                    Ok(())
                }
            }
            Ok(Arc::new(FakeChannel {
                base: ChannelBase::new(config, bus, workspace, transcription_api_key),
                sent,
            }))
        }),
    ));

    let bus = MessageBus::new(8);
    let cfg: ChannelsConfig = serde_json::from_value(json!({
        "fakeplugin": {"enabled": true, "allowFrom": ["*"]}
    }))
    .unwrap();
    let manager = ChannelManager::new(cfg, bus.clone(), PathBuf::new()).unwrap();
    assert!(
        manager
            .enabled_channels()
            .contains(&"fakeplugin".to_string())
    );
    manager.start_all().await.unwrap();
    bus.publish_outbound(OutboundMessage {
        channel: "fakeplugin".to_string(),
        chat_id: "room".to_string(),
        content: "hello".to_string(),
        reply_to: None,
        media: Vec::new(),
        reasoning_content: None,
        metadata: BTreeMap::new(),
    })
    .await
    .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    manager.stop_all().await.unwrap();
    assert_eq!(sent.lock().unwrap().as_slice(), &["hello"]);
    clear_plugins();
}

#[test]
fn builtin_local_channel_default_config() {
    let cfg = LocalChannel::default_config();
    assert_eq!(cfg["enabled"], json!(false));
    assert_eq!(cfg["allowFrom"], json!(["*"]));
}
