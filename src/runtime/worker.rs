use std::collections::HashMap;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use anyhow::Result;
use tokio::sync::{Mutex, Semaphore};
use tokio::task::JoinHandle;

use crate::engine::AgentLoop;
use crate::storage::{InboundMessage, MessageBus, OutboundMessage};
use crate::tools::MessageSendCallback;

type SessionLocks = Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>;

#[derive(Clone)]
pub struct AgentRuntime {
    agent: Arc<AgentLoop>,
    bus: MessageBus,
    running: Arc<AtomicBool>,
    task: Arc<Mutex<Option<JoinHandle<()>>>>,
    concurrency_semaphore: Arc<Semaphore>,
    session_locks: SessionLocks,
}

impl AgentRuntime {
    pub fn new(agent: Arc<AgentLoop>, bus: MessageBus, max_concurrent_requests: usize) -> Self {
        let publish_bus = bus.clone();
        let callback: MessageSendCallback = Arc::new(move |msg| {
            let bus = publish_bus.clone();
            Box::pin(async move {
                bus.publish_outbound(msg).await?;
                Ok(())
            })
        });
        agent.set_message_sender(Some(callback));
        let progress_bus = bus.clone();
        let progress_callback: MessageSendCallback = Arc::new(move |msg| {
            let bus = progress_bus.clone();
            Box::pin(async move {
                bus.publish_outbound(msg).await?;
                Ok(())
            })
        });
        agent.set_progress_sender(Some(progress_callback));
        agent.set_runtime_bus(bus.clone());
        let permits = if max_concurrent_requests == 0 {
            usize::MAX
        } else {
            max_concurrent_requests
        };
        Self {
            agent,
            bus,
            running: Arc::new(AtomicBool::new(false)),
            task: Arc::new(Mutex::new(None)),
            concurrency_semaphore: Arc::new(Semaphore::new(permits)),
            session_locks: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn bus(&self) -> MessageBus {
        self.bus.clone()
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    async fn get_session_lock(&self, session_key: &str) -> Arc<Mutex<()>> {
        let mut locks = self.session_locks.lock().await;
        locks
            .entry(session_key.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    pub async fn start(&self) -> Result<()> {
        if self.running.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let this = self.clone();
        let handle = tokio::spawn(async move {
            while this.running.load(Ordering::SeqCst) {
                let Some(msg) = this.bus.consume_inbound().await else {
                    break;
                };
                let session_key = msg.session_key();
                let this_clone = this.clone();
                tokio::spawn(async move {
                    if is_stop_command(&msg.content) {
                        this_clone.process_message(msg).await;
                        return;
                    }

                    let _permit = match this_clone.concurrency_semaphore.acquire().await {
                        Ok(permit) => permit,
                        Err(_) => return,
                    };
                    let session_lock = this_clone.get_session_lock(&session_key).await;
                    let _session_guard = session_lock.lock().await;

                    this_clone.process_message(msg).await;
                });
            }
        });
        *self.task.lock().await = Some(handle);
        Ok(())
    }

    pub async fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(handle) = self.task.lock().await.take() {
            handle.abort();
        }
        self.agent.set_message_sender(None);
        self.agent.set_progress_sender(None);
    }

    async fn process_message(&self, msg: InboundMessage) {
        match self.agent.process_inbound(msg).await {
            Ok(Some(outbound)) => {
                let _ = self.bus.publish_outbound(outbound).await;
            }
            Ok(None) => {}
            Err(err) => {
                let _ = self
                    .bus
                    .publish_outbound(OutboundMessage {
                        channel: "system".to_string(),
                        chat_id: "runtime".to_string(),
                        content: format!("Error processing inbound message: {err}"),
                        reply_to: None,
                        media: Vec::new(),
                        reasoning_content: None,
                        metadata: Default::default(),
                    })
                    .await;
            }
        }
    }
}

fn is_stop_command(content: &str) -> bool {
    matches!(
        content.trim().to_ascii_lowercase().as_str(),
        "/stop" | "stop" | "[stop]"
    )
}
