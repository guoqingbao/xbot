use std::collections::{BTreeMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use futures::future::join_all;
use regex::Regex;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::Semaphore;
use tokio::time::Instant as TokioInstant;

use crate::config::{ExecToolConfig, WebSearchConfig};
use crate::cron::CronService;
use crate::engine::{
    CompletedSubagentResult, ContextBuilder, MemoryConsolidator, MemoryEntry, MemoryEntryKind,
    SkillsLoader, SubagentManager,
};
use crate::integrations::mcp::register_mcp_tools;
use crate::providers::{LlmResponse, ProviderModelInfo, SharedProvider, TextStreamCallback};
use crate::storage::{
    ChatMessage, InboundMessage, MessageBus, OutboundMessage, Session, SessionManager,
};
use crate::tools::{
    CronTool, EditFileTool, ExecTool, GrepFilesTool, ListDirTool, MessageSendCallback, MessageTool,
    ReadFileTool, SpawnTool, ToolOutput, ToolRegistry, WaitSubagentsTool, WebFetchTool,
    WebSearchTool, WriteFileTool,
};
use crate::util::{build_status_content, truncate_chars_ellipsis, workspace_state_dir};

pub type ModelSwitchCallback = Arc<dyn Fn(String, Option<usize>) -> Result<()> + Send + Sync>;

const NANOBOT_STYLE_HELP: &str = "Available commands:\n\
  /help     - Show this help message\n\
  /status   - Show current session status\n\
  /new      - Clear current session and start fresh\n\
  /stop     - Cancel current processing\n\
  /model    - Switch model (e.g. /model gpt-4.1)\n\
  /memorize - Save important facts to long-term memory";
const SUBAGENT_WAIT_TIMEOUT: Duration = Duration::from_secs(300);
const CONTEXT_COMPRESSION_THRESHOLD_PERCENT: usize = 90;
const CONTEXT_COMPRESSION_TARGET_DIVISOR: usize = 10;

#[derive(Clone, Copy)]
struct MessageRecord {
    content_hash: u64,
    timestamp: TokioInstant,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentSnapshot {
    pub model: String,
    pub workspace: String,
    pub uptime_seconds: u64,
    pub max_iterations: usize,
    pub context_window_tokens: usize,
    pub session_count: usize,
    pub running_subagents: usize,
    pub last_prompt_tokens: usize,
    pub last_completion_tokens: usize,
    pub last_cached_tokens: usize,
}

pub struct AgentLoop {
    provider: SharedProvider,
    workspace: PathBuf,
    model: String,
    max_iterations: usize,
    context_window_tokens: usize,
    context: ContextBuilder,
    sessions: Mutex<SessionManager>,
    tools: ToolRegistry,
    memory: MemoryConsolidator,
    subagents: SubagentManager,
    message_tool: Arc<MessageTool>,
    progress_sender: Arc<Mutex<Option<MessageSendCallback>>>,
    spawn_tool: Arc<SpawnTool>,
    wait_subagents_tool: Arc<WaitSubagentsTool>,
    cron_tool: Option<Arc<CronTool>>,
    start_time: Instant,
    last_usage: Mutex<(usize, usize, usize)>,
    last_context_prompt_tokens: Mutex<usize>,
    tool_semaphore: Arc<Semaphore>,
    cancellations: Arc<Mutex<HashSet<String>>>,
    active_turns: Arc<Mutex<BTreeMap<String, usize>>>,
    stop_notifications: Arc<Mutex<BTreeMap<String, StopNotification>>>,
    announced_sessions: Arc<Mutex<HashSet<String>>>,
    model_switch_callback: Arc<Mutex<Option<ModelSwitchCallback>>>,
    auto_task_summary_enabled: AtomicBool,
    memory_enabled: AtomicBool,
    approval_callback: Arc<Mutex<Option<crate::tools::ApprovalCallback>>>,
    always_allow: Arc<Mutex<bool>>,
    steer_rx: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<String>>>>,
    steer_tx: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<String>>>>,
    message_rate_limiter: Arc<Mutex<BTreeMap<String, MessageRecord>>>,
    duplicate_message_window_seconds: u64,
}

impl AgentLoop {
    pub async fn new(
        provider: SharedProvider,
        workspace: impl AsRef<Path>,
        model: Option<String>,
        max_iterations: usize,
        max_concurrent_tools: usize,
        context_window_tokens: usize,
        max_memory_bytes: usize,
        web_search: WebSearchConfig,
        web_proxy: Option<String>,
        exec: ExecToolConfig,
        restrict_to_workspace: bool,
        cron_service: Option<CronService>,
        mcp_servers: &BTreeMap<String, crate::config::McpServerConfig>,
    ) -> Result<Self> {
        Self::new_with_subagent_provider(
            provider,
            workspace,
            model,
            None,
            None,
            max_iterations,
            max_concurrent_tools,
            context_window_tokens,
            max_memory_bytes,
            web_search,
            web_proxy,
            exec,
            restrict_to_workspace,
            cron_service,
            true,
            mcp_servers,
        )
        .await
    }

    pub async fn new_with_subagent_provider(
        provider: SharedProvider,
        workspace: impl AsRef<Path>,
        model: Option<String>,
        subagent_provider: Option<SharedProvider>,
        subagent_model: Option<String>,
        max_iterations: usize,
        max_concurrent_tools: usize,
        context_window_tokens: usize,
        max_memory_bytes: usize,
        web_search: WebSearchConfig,
        web_proxy: Option<String>,
        exec: ExecToolConfig,
        restrict_to_workspace: bool,
        cron_service: Option<CronService>,
        memory_enabled: bool,
        mcp_servers: &BTreeMap<String, crate::config::McpServerConfig>,
    ) -> Result<Self> {
        let workspace = workspace.as_ref().to_path_buf();
        let context = ContextBuilder::new(&workspace, max_memory_bytes)?;
        context.set_memory_enabled(memory_enabled);
        context.set_task_summary_guidance_enabled(memory_enabled);
        let sessions = SessionManager::new(&workspace)?;
        let memory = MemoryConsolidator::new(&workspace, context_window_tokens, max_memory_bytes)?;
        let resolved_model = model
            .clone()
            .unwrap_or_else(|| provider.default_model().to_string());
        let approval_callback = Arc::new(Mutex::new(None));
        let always_allow = Arc::new(Mutex::new(false));
        let cancellations: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
        let resolved_context_window_tokens = provider
            .list_models()
            .await
            .ok()
            .and_then(|models| find_model_context_window_tokens(&models, &resolved_model))
            .unwrap_or(context_window_tokens);
        let subagents = SubagentManager::new(
            subagent_provider.unwrap_or_else(|| provider.clone()),
            workspace.clone(),
            MessageBus::new(64),
            subagent_model.unwrap_or_else(|| resolved_model.clone()),
            web_search.clone(),
            web_proxy.clone(),
            exec.clone(),
            restrict_to_workspace,
            memory_enabled,
            resolved_context_window_tokens,
            approval_callback.clone(),
            always_allow.clone(),
            cancellations.clone(),
        );
        let mut tools = ToolRegistry::new();
        let allowed_dir = restrict_to_workspace.then(|| workspace.clone());
        let blocked_dirs = {
            let mut dirs = Vec::new();
            
            // Block memory directory when memory is not enabled
            if !memory_enabled {
                dirs.push(workspace_state_dir(&workspace).join("memory"));
            }
            
            // Block sessions directory to prevent xbot from reading its own sessions
            dirs.push(workspace_state_dir(&workspace).join("sessions"));
            
            // Block tui_input_history.json to prevent xbot from reading its own input history
            dirs.push(workspace_state_dir(&workspace).join("tui_input_history.json"));
            
            dirs
        };
        tools.register(Arc::new(
            ReadFileTool::new(Some(workspace.clone()), allowed_dir.clone(), vec![])
                .with_blocked_dirs(blocked_dirs.clone()),
        ));
        tools.register(Arc::new(
            WriteFileTool::new(Some(workspace.clone()), allowed_dir.clone())
                .with_blocked_dirs(blocked_dirs.clone()),
        ));
        tools.register(Arc::new(
            EditFileTool::new(Some(workspace.clone()), allowed_dir.clone())
                .with_blocked_dirs(blocked_dirs.clone()),
        ));
        tools.register(Arc::new(
            ListDirTool::new(Some(workspace.clone()), allowed_dir.clone())
                .with_blocked_dirs(blocked_dirs.clone()),
        ));
        tools.register(Arc::new(GrepFilesTool::new(
            Some(workspace.clone()),
            allowed_dir.clone(),
        )));
        if exec.enable {
            tools.register(Arc::new(
                ExecTool::new(
                    exec.timeout,
                    Some(workspace.clone()),
                    restrict_to_workspace,
                    exec.path_append.clone(),
                )
                .with_blocked_dirs(blocked_dirs.clone()),
            ));
        }
        tools.register(Arc::new(WebSearchTool::new(web_search, web_proxy.clone())));
        tools.register(Arc::new(WebFetchTool::new(50_000, web_proxy)));
        let message_tool = Arc::new(MessageTool::new(None));
        tools.register(message_tool.clone());
        let spawn_tool = Arc::new(SpawnTool::new(subagents.clone()));
        tools.register(spawn_tool.clone());
        let wait_subagents_tool = Arc::new(WaitSubagentsTool::new(subagents.clone()));
        tools.register(wait_subagents_tool.clone());
        let cron_tool = cron_service.map(|service| Arc::new(CronTool::new(service)));
        if let Some(cron_tool) = &cron_tool {
            tools.register(cron_tool.clone());
        }
        register_mcp_tools(&mut tools, mcp_servers).await?;

        let tool_semaphore = Arc::new(Semaphore::new(max_concurrent_tools.max(1)));

        Ok(Self {
            provider: provider.clone(),
            workspace,
            model: model.unwrap_or_else(|| provider.default_model().to_string()),
            max_iterations,
            context_window_tokens: resolved_context_window_tokens,
            context,
            sessions: Mutex::new(sessions),
            tools,
            memory,
            subagents,
            message_tool,
            progress_sender: Arc::new(Mutex::new(None)),
            spawn_tool,
            wait_subagents_tool,
            cron_tool,
            start_time: Instant::now(),
            last_usage: Mutex::new((0, 0, 0)),
            last_context_prompt_tokens: Mutex::new(0),
            tool_semaphore,
            cancellations,
            active_turns: Arc::new(Mutex::new(BTreeMap::new())),
            stop_notifications: Arc::new(Mutex::new(BTreeMap::new())),
            announced_sessions: Arc::new(Mutex::new(HashSet::new())),
            model_switch_callback: Arc::new(Mutex::new(None)),
            auto_task_summary_enabled: AtomicBool::new(memory_enabled),
            memory_enabled: AtomicBool::new(memory_enabled),
            approval_callback,
            always_allow,
            steer_rx: Arc::new(Mutex::new(None)),
            steer_tx: Arc::new(Mutex::new(None)),
            message_rate_limiter: Arc::new(Mutex::new(BTreeMap::new())),
            duplicate_message_window_seconds: 2, // Default value, can be overridden by config
        })
    }

    pub fn set_approval_callback(&self, callback: Option<crate::tools::ApprovalCallback>) {
        *self
            .approval_callback
            .lock()
            .expect("approval callback lock poisoned") = callback;
    }

    pub fn set_message_sender(&self, callback: Option<MessageSendCallback>) {
        self.message_tool.set_send_callback(callback);
    }

    pub fn set_progress_sender(&self, callback: Option<MessageSendCallback>) {
        *self
            .progress_sender
            .lock()
            .expect("progress callback lock poisoned") = callback;
    }

pub fn set_subagent_notification_callback(
    &self,
    callback: Option<crate::engine::subtasks::SubagentNotificationCallback>,
) {
    self.subagents.set_notification_callback(callback);
}

fn check_message_rate_limit(&self, session_key: &str, content: &str, window_seconds: u64) -> Result<()> {
    let mut limiter = self.message_rate_limiter.lock().expect("rate limiter lock poisoned");
    let now = TokioInstant::now();
    
    // Clean up old entries (older than window_seconds * 2)
    limiter.retain(|_, record| {
        now.duration_since(record.timestamp).as_secs() < window_seconds * 2
    });
    
    // Check for duplicate content within the window
    let content_hash = {
        let mut hasher = DefaultHasher::new();
        content.hash(&mut hasher);
        hasher.finish()
    };
    
    if let Some(record) = limiter.get(session_key) {
        if record.content_hash == content_hash {
            let elapsed = now.duration_since(record.timestamp);
            if elapsed.as_secs() < window_seconds {
                // Log the duplicate detection
                eprintln!(
                    "WARNING: Duplicate message detected for session {}: content hash {} within {} seconds (elapsed: {:?})",
                    session_key,
                    content_hash,
                    window_seconds,
                    elapsed
                );
                return Err(anyhow::anyhow!("Duplicate message detected within {} second window", window_seconds));
            }
        }
    }
    
    // Store the new content hash and time
    limiter.insert(session_key.to_string(), MessageRecord {
        content_hash,
        timestamp: now,
    });
    Ok(())
}

pub fn setup_steer_channel(&self) -> tokio::sync::mpsc::UnboundedSender<String> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        *self.steer_rx.lock().expect("steer_rx lock poisoned") = Some(rx);
        *self.steer_tx.lock().expect("steer_tx lock poisoned") = Some(tx.clone());
        tx
    }

    pub fn steer_sender(&self) -> Option<tokio::sync::mpsc::UnboundedSender<String>> {
        self.steer_tx
            .lock()
            .expect("steer_tx lock poisoned")
            .clone()
    }

    pub fn clear_steer_channel(&self) {
        *self.steer_rx.lock().expect("steer_rx lock poisoned") = None;
        *self.steer_tx.lock().expect("steer_tx lock poisoned") = None;
    }

    pub async fn cancel_subagents(&self, session_key: &str) {
        self.subagents.cancel_by_session(session_key).await;
    }

    pub fn request_cancellation(&self, session_key: &str) {
        self.cancellations
            .lock()
            .expect("cancellations lock poisoned")
            .insert(session_key.to_string());
    }

    pub fn is_session_cancelled(&self, session_key: &str) -> bool {
        self.cancellations
            .lock()
            .expect("cancellations lock poisoned")
            .contains(session_key)
    }

    pub fn cancel_session(&self, session_key: &str) {
        self.cancellations
            .lock()
            .expect("cancellations lock poisoned")
            .remove(session_key);
    }

    /// Best-effort save of the current session, trimming any trailing
    /// unpaired tool-call / tool-result messages so the stored history
    /// is always valid for provider replay.
    pub fn persist_session_safe(&self, session_key: &str) {
        let mut sessions = self.sessions.lock().expect("session manager lock poisoned");
        if let Ok(mut session) = sessions.get_or_create(session_key) {
            trim_unpaired_tool_tail(&mut session.messages);
            let _ = sessions.save(&session);
        }
    }

    pub fn set_model_switch_callback(&self, callback: Option<ModelSwitchCallback>) {
        *self
            .model_switch_callback
            .lock()
            .expect("model switch callback lock poisoned") = callback;
    }

    pub fn set_auto_task_summary_enabled(&self, enabled: bool) {
        self.auto_task_summary_enabled
            .store(enabled, Ordering::SeqCst);
        self.context.set_task_summary_guidance_enabled(enabled);
    }

    pub fn set_memory_enabled(&self, enabled: bool) {
        self.memory_enabled.store(enabled, Ordering::SeqCst);
        self.context.set_memory_enabled(enabled);
        self.set_auto_task_summary_enabled(enabled);
    }

    fn memory_enabled(&self) -> bool {
        self.memory_enabled.load(Ordering::SeqCst)
    }

    fn maybe_consolidate_session(
        &self,
        session: &mut Session,
        context_window_tokens: usize,
    ) -> Result<()> {
        if self.memory_enabled() {
            self.memory
                .maybe_consolidate_by_tokens(session, context_window_tokens)?;
        }
        Ok(())
    }

    pub fn set_runtime_bus(&self, bus: MessageBus) {
        self.subagents.set_bus(bus);
    }

    async fn prepare_session(
        &self,
        msg: &InboundMessage,
        target: &ProgressTarget,
        session_key: &str,
        trimmed: &str,
        trimmed_lower: &str,
    ) -> Result<SessionSetup> {
        self.refresh_session_model_metadata(session_key).await?;

        if let Some(model_arg) = parse_model_command(trimmed) {
            return self
                .handle_model_command(msg, session_key, model_arg.as_deref())
                .await;
        }

        let mut sessions = self.sessions.lock().expect("session manager lock poisoned");
        let mut session = sessions.get_or_create(session_key)?;
        let active_model = self.session_model(&session);
        let context_window_tokens = self.session_context_window_tokens(&session);
        let session_notice = if should_announce_backend_session(msg, trimmed_lower) {
            self.register_session_announcement(session_key)
                .then(|| self.format_backend_session_notice(&session))
        } else {
            None
        };

        match trimmed_lower {
            "/new" | "new" | "/clear" | "clear" | "[clear]" => {
                self.maybe_consolidate_session(&mut session, context_window_tokens)?;
                session.clear();
                sessions.save(&session)?;
                self.subagents.reset_session(session_key);
                self.reset_session_announcement(session_key);
                *self.last_usage.lock().expect("usage lock poisoned") = (0, 0, 0);
                *self
                    .last_context_prompt_tokens
                    .lock()
                    .expect("context prompt lock poisoned") = 0;
                return Ok(SessionSetup {
                    response: Some(
                        target.outbound("New session started. Previous messages were cleared."),
                    ),
                    session_notice: None,
                    active_model,
                    context_window_tokens,
                });
            }
            "/status" | "status" => {
                return Ok(SessionSetup {
                    response: Some(self.status_response(msg, &session)),
                    session_notice: None,
                    active_model,
                    context_window_tokens,
                });
            }
            "/help" | "help" => {
                return Ok(SessionSetup {
                    response: Some(target.outbound(NANOBOT_STYLE_HELP)),
                    session_notice: None,
                    active_model,
                    context_window_tokens,
                });
            }
            _ => {
                if let Some(new_key) = trimmed_lower.strip_prefix("/switch ") {
                    let new_key = new_key.trim().to_string();
                    self.subagents.reset_session(session_key);
                    self.reset_session_announcement(session_key);
                    *self.last_usage.lock().expect("usage lock poisoned") = (0, 0, 0);
                    *self
                        .last_context_prompt_tokens
                        .lock()
                        .expect("context prompt lock poisoned") = 0;
                    let switched = sessions.get_or_create(&new_key)?;
                    let msg_count = switched.get_history(0).len();
                    return Ok(SessionSetup {
                        response: Some(
                            target
                                .outbound(
                                    format!("Switched to session ({} messages).", msg_count,),
                                ),
                        ),
                        session_notice: None,
                        active_model: self.session_model(&switched),
                        context_window_tokens: self.session_context_window_tokens(&switched),
                    });
                }
            }
        }

        self.maybe_consolidate_session(&mut session, context_window_tokens)?;
        sessions.put(session);
        Ok(SessionSetup {
            response: None,
            session_notice,
            active_model,
            context_window_tokens,
        })
    }

    async fn refresh_session_model_metadata(&self, session_key: &str) -> Result<()> {
        let (session_model, stored_context_window_tokens) = {
            let mut sessions = self.sessions.lock().expect("session manager lock poisoned");
            let session = sessions.get_or_create(session_key)?;
            (
                session
                    .metadata
                    .get(SESSION_MODEL_KEY)
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
                session
                    .metadata
                    .get(SESSION_CONTEXT_WINDOW_KEY)
                    .and_then(Value::as_u64)
                    .map(|value| value as usize)
                    .filter(|value| *value > 0),
            )
        };
        let Some(session_model) = session_model else {
            return Ok(());
        };

        let models = match self.provider.list_models().await {
            Ok(models) if !models.is_empty() => models,
            _ => {
                // Provider doesn't support listing or returned empty; if the
                // session model differs from the configured default, reset it
                // so a stale model name from a previous provider isn't reused.
                if session_model != self.model {
                    let mut sessions = self.sessions.lock().expect("session manager lock poisoned");
                    let mut session = sessions.get_or_create(session_key)?;
                    session.metadata.insert(
                        SESSION_MODEL_KEY.to_string(),
                        Value::String(self.model.clone()),
                    );
                    sessions.save(&session)?;
                }
                return Ok(());
            }
        };
        let (new_model_id, new_context_window_tokens) =
            if let Some(resolved) = resolve_runtime_model_info(&models, &session_model) {
                (resolved.id.clone(), resolved.context_window_tokens)
            } else {
                // Session model not found in provider — reset to configured default
                let fallback = resolve_runtime_model_info(&models, &self.model)
                    .map(|m| (m.id.clone(), m.context_window_tokens))
                    .unwrap_or_else(|| (self.model.clone(), None));
                fallback
            };
        let model_changed = new_model_id != session_model;
        let context_changed = new_context_window_tokens
            .is_some_and(|value| Some(value) != stored_context_window_tokens);
        if !model_changed && !context_changed {
            return Ok(());
        }

        {
            let mut sessions = self.sessions.lock().expect("session manager lock poisoned");
            let mut session = sessions.get_or_create(session_key)?;
            session.metadata.insert(
                SESSION_MODEL_KEY.to_string(),
                Value::String(new_model_id.clone()),
            );
            if let Some(context_window_tokens) = new_context_window_tokens {
                session.metadata.insert(
                    SESSION_CONTEXT_WINDOW_KEY.to_string(),
                    Value::from(context_window_tokens as u64),
                );
            }
            sessions.save(&session)?;
        }

        if let Some(callback) = self
            .model_switch_callback
            .lock()
            .expect("model switch callback lock poisoned")
            .clone()
        {
            let _ = callback(new_model_id.clone(), new_context_window_tokens);
        }

        Ok(())
    }

    async fn handle_model_command(
        &self,
        msg: &InboundMessage,
        session_key: &str,
        requested_model: Option<&str>,
    ) -> Result<SessionSetup> {
        let models = self.provider.list_models().await?;
        if models.is_empty() {
            return Err(anyhow::anyhow!("provider returned no models"));
        }

        let current_model = {
            let mut sessions = self.sessions.lock().expect("session manager lock poisoned");
            let session = sessions.get_or_create(session_key)?;
            self.session_model(&session)
        };

        if requested_model.is_none() {
            let mut lines = vec![format!("Current model: {current_model}")];
            if let Some(context_window_tokens) =
                find_model_context_window_tokens(&models, &current_model)
            {
                lines.push(format!("Context window: {context_window_tokens}"));
            }
            lines.push("Available models:".to_string());
            lines.extend(models.iter().map(|model| format!("- {}", model.id)));
            return Ok(SessionSetup {
                response: Some(ProgressTarget::from_inbound(msg).outbound(lines.join("\n"))),
                session_notice: None,
                active_model: current_model.clone(),
                context_window_tokens: find_model_context_window_tokens(&models, &current_model)
                    .unwrap_or(self.context_window_tokens),
            });
        }

        let requested_model = requested_model.unwrap_or_default();
        let selected_model =
            resolve_model_selection(&models, requested_model).ok_or_else(|| {
                anyhow::anyhow!("model '{requested_model}' was not found in provider /models")
            })?;
        let context_window_tokens = selected_model
            .context_window_tokens
            .unwrap_or(self.context_window_tokens);
        if let Some(callback) = self
            .model_switch_callback
            .lock()
            .expect("model switch callback lock poisoned")
            .clone()
        {
            callback(
                selected_model.id.clone(),
                selected_model.context_window_tokens,
            )?;
        }
        {
            let mut sessions = self.sessions.lock().expect("session manager lock poisoned");
            let mut session = sessions.get_or_create(session_key)?;
            session.metadata.insert(
                SESSION_MODEL_KEY.to_string(),
                Value::String(selected_model.id.clone()),
            );
            if let Some(context_window_tokens) = selected_model.context_window_tokens {
                session.metadata.insert(
                    SESSION_CONTEXT_WINDOW_KEY.to_string(),
                    Value::from(context_window_tokens as u64),
                );
            } else {
                session.metadata.remove(SESSION_CONTEXT_WINDOW_KEY);
            }
            sessions.save(&session)?;
        }
        Ok(SessionSetup {
            response: Some(ProgressTarget::from_inbound(msg).outbound(format!(
                "Model switched to {}{}",
                selected_model.id,
                selected_model
                    .context_window_tokens
                    .map(|value| format!(" (context window {value})"))
                    .unwrap_or_default()
            ))),
            session_notice: None,
            active_model: selected_model.id.clone(),
            context_window_tokens,
        })
    }

    fn session_model(&self, session: &Session) -> String {
        session
            .metadata
            .get(SESSION_MODEL_KEY)
            .and_then(Value::as_str)
            .filter(|model| !model.trim().is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| self.model.clone())
    }

    fn session_context_window_tokens(&self, session: &Session) -> usize {
        session
            .metadata
            .get(SESSION_CONTEXT_WINDOW_KEY)
            .and_then(Value::as_u64)
            .map(|value| value as usize)
            .filter(|value| *value > 0)
            .unwrap_or(self.context_window_tokens)
    }

    fn register_session_announcement(&self, session_key: &str) -> bool {
        self.announced_sessions
            .lock()
            .expect("announced sessions lock poisoned")
            .insert(session_key.to_string())
    }

    fn reset_session_announcement(&self, session_key: &str) -> bool {
        self.announced_sessions
            .lock()
            .expect("announced sessions lock poisoned")
            .remove(session_key)
    }

    pub async fn process_direct(
        &self,
        content: &str,
        session_key: &str,
        channel: &str,
        chat_id: &str,
    ) -> Result<Option<OutboundMessage>> {
        self.process_direct_stream(content, session_key, channel, chat_id, None, None, None)
            .await
    }

    pub async fn process_direct_stream(
        &self,
        content: &str,
        session_key: &str,
        channel: &str,
        chat_id: &str,
        media: Option<&[String]>,
        text_stream: Option<TextStreamCallback>,
        reasoning_stream: Option<crate::providers::ReasoningStreamCallback>,
    ) -> Result<Option<OutboundMessage>> {
        self.process_inbound_with_stream(
            InboundMessage {
                channel: channel.to_string(),
                sender_id: "user".to_string(),
                chat_id: chat_id.to_string(),
                content: content.to_string(),
                timestamp: chrono::Utc::now(),
                media: media.map(|m| m.to_vec()).unwrap_or_default(),
                metadata: BTreeMap::new(),
                session_key_override: Some(session_key.to_string()),
            },
            text_stream,
            reasoning_stream,
        )
        .await
    }

    pub async fn process_inbound(&self, msg: InboundMessage) -> Result<Option<OutboundMessage>> {
        self.process_inbound_with_stream(msg, None, None).await
    }

    async fn process_inbound_with_stream(
        &self,
        msg: InboundMessage,
        text_stream: Option<TextStreamCallback>,
        reasoning_stream: Option<crate::providers::ReasoningStreamCallback>,
    ) -> Result<Option<OutboundMessage>> {
        if msg.channel == "system" {
            return self.process_system_inbound(msg).await;
        }
        let target = ProgressTarget::from_inbound(&msg);
        let trimmed = msg.content.trim();
        let trimmed_lower = trimmed.to_lowercase();
        if trimmed_lower == "/stop" || trimmed_lower == "stop" || trimmed_lower == "[stop]" {
            return self.handle_stop_signal(&msg, &target).await;
        }
        if let Some(memory_input) = parse_memorize_command(&msg.content) {
            return match self.handle_memorize_signal(&target, &memory_input).await {
                Ok(response) => Ok(response),
                Err(err) => Ok(Some(
                    target.outbound(format!("Unable to memorize input: {err}")),
                )),
            };
        }

        let session_key = msg.session_key();

        // Rate limiting: prevent duplicate messages within configured window
        if self.check_message_rate_limit(&session_key, trimmed, self.duplicate_message_window_seconds).is_err() {
            return Ok(Some(target.outbound(format!("Message rate limited. Please wait {} seconds before sending another message.", self.duplicate_message_window_seconds))));
        }

        // Immediate cancellation check: if user just sent a stop command, don't start a new turn
        if self.is_cancellation_pending(&session_key) {
            if self.has_active_turn(&session_key) {
                return Ok(None);
            }
            self.clear_cancellation(&session_key);
        }

        let session_setup = self
            .prepare_session(&msg, &target, &session_key, trimmed, &trimmed_lower)
            .await
            .or_else(|err| {
                if let Some(action) = special_command_action(&trimmed_lower) {
                    Ok(SessionSetup {
                        response: Some(target.outbound(format!("Unable to {action}: {err}"))),
                        session_notice: None,
                        active_model: self.model.clone(),
                        context_window_tokens: self.context_window_tokens,
                    })
                } else {
                    Err(err)
                }
            })?;
        let SessionSetup {
            response,
            session_notice,
            active_model,
            context_window_tokens,
        } = session_setup;
        if let Some(response) = response {
            return Ok(Some(response));
        }
        if let Some(notice) = session_notice {
            self.send_runtime_reply(&target, notice).await;
        }

        self.message_tool.set_context(
            &msg.channel,
            &msg.chat_id,
            msg.metadata
                .get("message_id")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
        );
        self.message_tool.start_turn();
        self.spawn_tool
            .set_context(&msg.channel, &msg.chat_id, &session_key, &msg.metadata);
        self.wait_subagents_tool.set_context(&session_key);
        if let Some(cron_tool) = &self.cron_tool {
            cron_tool.set_context(&msg.channel, &msg.chat_id);
        }

        let session_key = msg.session_key();
        
        // Get or build the static system prompt for this session
        let static_system_prompt: Option<String> = {
            let mut sessions = self.sessions.lock().expect("session manager lock poisoned");
            let mut session = sessions.get_or_create(&session_key)?;
            
            if let Some(ref prompt) = session.system_prompt {
                // Use existing static system prompt
                Some(prompt.clone())
            } else {
                // Build a new system prompt and store it
                let new_prompt = self.context.build_static_system_prompt()?;
                session.system_prompt = Some(new_prompt.clone());
                sessions.save(&session)?;
                Some(new_prompt)
            }
        };
        
        let history = {
            let mut sessions = self.sessions.lock().expect("session manager lock poisoned");
            sessions.get_or_create(&session_key)?.get_history(0)
        };
        let initial_messages = self.context.build_messages(
            history,
            &msg.content,
            Some(&msg.media),
            Some(&msg.channel),
            Some(&msg.chat_id),
            "user",
            static_system_prompt.as_deref(),
            Some(&self.tools.definitions()),
        )?;

        let loop_result = {
            let _guard = ActiveTurnGuard::new(self.active_turns.clone(), session_key.clone());
            self.run_agent_loop(
                &session_key,
                &active_model,
                context_window_tokens,
                initial_messages.clone(),
                text_stream,
                reasoning_stream,
                Some(target.clone()),
            )
            .await
        };
        let (
            final_content,
            all_messages,
            interrupted,
            final_reasoning_content,
            completed_normally,
            context_compressed,
        ) = match loop_result {
            Ok(result) => result,
            Err(err) => {
                self.persist_session_messages(&session_key, &initial_messages)?;
                self.finalize_stop_state(
                    &session_key,
                    false,
                    Some(format!("Unable to stop task: {err}")),
                )
                .await;
                return Err(err);
            }
        };

        {
            let mut sessions = self.sessions.lock().expect("session manager lock poisoned");
            let mut session = sessions.get_or_create(&session_key)?;
            if context_compressed {
                self.replace_session_messages(&mut session, &all_messages);
            } else {
                self.save_turn(&mut session, &all_messages)?;
            }
            self.apply_latest_context_usage_metadata(&mut session);
            self.maybe_consolidate_session(&mut session, context_window_tokens)?;
            sessions.save(&session)?;
        }

        if interrupted {
            self.finalize_stop_state(&session_key, true, None).await;
            return Ok(None);
        }
        self.finalize_stop_state(
            &session_key,
            false,
            Some(
                "Unable to stop task: task already completed before cancellation took effect."
                    .to_string(),
            ),
        )
        .await;
        if completed_normally && self.auto_task_summary_enabled.load(Ordering::SeqCst) {
            self.record_completed_task_memory(
                &msg.content,
                final_content.as_deref(),
                &all_messages,
                Some(&target),
            )
            .await;
        }

        if self.message_tool.sent_in_turn() {
            return Ok(None);
        }
        let content = final_content.unwrap_or_else(|| {
            "I've completed processing but have no response to give.".to_string()
        });
        Ok(Some(OutboundMessage {
            channel: target.channel,
            chat_id: target.chat_id,
            content,
            reply_to: None,
            media: Vec::new(),
            reasoning_content: final_reasoning_content,
            metadata: target.metadata,
        }))
    }

    async fn process_system_inbound(&self, msg: InboundMessage) -> Result<Option<OutboundMessage>> {
        if msg.sender_id == "subagent" {
            if let Some(task_id) = msg.metadata.get("task_id").and_then(Value::as_str) {
                if self.subagents.take_consumed_result(task_id) {
                    return Ok(None);
                }
            }
        }

        let (channel, chat_id) = msg
            .chat_id
            .split_once(':')
            .map(|(channel, chat_id)| (channel.to_string(), chat_id.to_string()))
            .unwrap_or_else(|| ("cli".to_string(), msg.chat_id.clone()));
        let progress_target = ProgressTarget {
            channel: channel.clone(),
            chat_id: chat_id.clone(),
            session_key: msg
                .session_key_override
                .clone()
                .unwrap_or_else(|| format!("{channel}:{chat_id}")),
            metadata: msg.metadata.clone(),
        };
        let session_key = progress_target.session_key.clone();

        // Get or build the static system prompt for this session
        let static_system_prompt: Option<String> = {
            let mut sessions = self.sessions.lock().expect("session manager lock poisoned");
            let mut session = sessions.get_or_create(&session_key)?;
            
            if let Some(ref prompt) = session.system_prompt {
                Some(prompt.clone())
            } else {
                let new_prompt = self.context.build_static_system_prompt()?;
                session.system_prompt = Some(new_prompt.clone());
                sessions.save(&session)?;
                Some(new_prompt)
            }
        };

        {
            let mut sessions = self.sessions.lock().expect("session manager lock poisoned");
            let mut session = sessions.get_or_create(&session_key)?;
            let context_window_tokens = self.session_context_window_tokens(&session);
            self.maybe_consolidate_session(&mut session, context_window_tokens)?;
            sessions.put(session);
        }

        self.message_tool.set_context(&channel, &chat_id, None);
        self.message_tool.start_turn();
        self.spawn_tool
            .set_context(&channel, &chat_id, &session_key, &msg.metadata);
        self.wait_subagents_tool.set_context(&session_key);
        let history = {
            let mut sessions = self.sessions.lock().expect("session manager lock poisoned");
            sessions.get_or_create(&session_key)?.get_history(0)
        };
        let active_model = {
            let mut sessions = self.sessions.lock().expect("session manager lock poisoned");
            let session = sessions.get_or_create(&session_key)?;
            self.session_model(&session)
        };
        let context_window_tokens = {
            let mut sessions = self.sessions.lock().expect("session manager lock poisoned");
            let session = sessions.get_or_create(&session_key)?;
            self.session_context_window_tokens(&session)
        };
        let initial_messages = self.context.build_messages(
            history,
            &msg.content,
            None,
            Some(&channel),
            Some(&chat_id),
            if msg.sender_id == "subagent" {
                "assistant"
            } else {
                "user"
            },
            static_system_prompt.as_deref(),
            Some(&self.tools.definitions()),
        )?;

        let loop_result = {
            let _guard = ActiveTurnGuard::new(self.active_turns.clone(), session_key.clone());
            self.run_agent_loop(
                &session_key,
                &active_model,
                context_window_tokens,
                initial_messages.clone(),
                None,
                None,
                Some(progress_target.clone()),
            )
            .await
        };
        let (
            final_content,
            all_messages,
            interrupted,
            _final_reasoning_content,
            _completed_normally,
            context_compressed,
        ) = match loop_result {
            Ok(result) => result,
            Err(err) => {
                self.persist_session_messages(&session_key, &initial_messages)?;
                self.finalize_stop_state(
                    &session_key,
                    false,
                    Some(format!("Unable to stop task: {err}")),
                )
                .await;
                return Err(err);
            }
        };

        {
            let mut sessions = self.sessions.lock().expect("session manager lock poisoned");
            let mut session = sessions.get_or_create(&session_key)?;
            if context_compressed {
                self.replace_session_messages(&mut session, &all_messages);
            } else {
                self.save_turn(&mut session, &all_messages)?;
            }
            self.apply_latest_context_usage_metadata(&mut session);
            self.maybe_consolidate_session(&mut session, context_window_tokens)?;
            sessions.save(&session)?;
        }

        if interrupted {
            self.finalize_stop_state(&session_key, true, None).await;
            return Ok(None);
        }
        self.finalize_stop_state(
            &session_key,
            false,
            Some(
                "Unable to stop task: task already completed before cancellation took effect."
                    .to_string(),
            ),
        )
        .await;

        let content = final_content.unwrap_or_else(|| "Background task completed.".to_string());
        Ok(Some(progress_target.outbound(content)))
    }

    async fn run_agent_loop(
        &self,
        session_key: &str,
        active_model: &str,
        context_window_tokens: usize,
        mut messages: Vec<ChatMessage>,
        text_stream: Option<TextStreamCallback>,
        reasoning_stream: Option<crate::providers::ReasoningStreamCallback>,
        progress_target: Option<ProgressTarget>,
    ) -> Result<(
        Option<String>,
        Vec<ChatMessage>,
        bool,
        Option<String>,
        bool,
        bool,
    )> {
        *self.last_usage.lock().expect("usage lock poisoned") = (0, 0, 0);
        *self
            .last_context_prompt_tokens
            .lock()
            .expect("context prompt lock poisoned") = 0;
        let mut final_content = None;
        let mut final_reasoning_content = None;
        let mut completed_normally = false;
        let mut context_compressed = false;
        let mut compression_pending = false;
        let mut empty_content_nudge_sent = false;
        let mut empty_output_nudge_count = 0_usize;
        let think_re = Regex::new(r"(?s)<think>.*?</think>").expect("valid think regex");
        let mut last_tool_call_fingerprint: Option<String> = None;
        let mut repeated_tool_call_streak = 0_usize;
        let mut same_tool_name_streak = 0_usize;
        let mut same_tool_nudges_sent = 0_usize;
        let mut last_tool_names: Option<String> = None;
        let mut last_assistant_content: Option<String> = None;

        let mut iteration = 0_usize;
        loop {
            // Check for cancellation at the start of the loop
            {
                if self
                    .cancellations
                    .lock()
                    .expect("cancellations lock poisoned")
                    .contains(session_key)
                {
                    return Ok((None, messages, true, None, false, context_compressed));
                }
            }

            if self.max_iterations > 0 && iteration >= self.max_iterations {
                break;
            }
            if compression_pending {
                messages = self
                    .compress_context_for_next_request(
                        messages,
                        active_model,
                        context_window_tokens,
                        progress_target.as_ref(),
                    )
                    .await?;
                context_compressed = true;
                compression_pending = false;
            }

            {
                let steer_messages: Vec<String> = {
                    let mut rx_guard = self.steer_rx.lock().expect("steer_rx lock poisoned");
                    let mut collected = Vec::new();
                    if let Some(ref mut rx) = *rx_guard {
                        while let Ok(msg) = rx.try_recv() {
                            collected.push(msg);
                        }
                    }
                    collected
                };
                for steer_msg in steer_messages {
                    self.send_steer_hint(progress_target.as_ref(), &steer_msg)
                        .await;
                    messages.push(ChatMessage::text("user", steer_msg));
                }
            }

            iteration += 1;
            let defs = self.tools.definitions();
            let response = self
                .provider
                .chat_with_retry_stream(
                    &messages,
                    Some(&defs),
                    Some(active_model),
                    None,
                    None,
                    text_stream.clone(),
                    reasoning_stream.clone(),
                )
                .await?;
            self.record_usage(&response);
            self.send_context_update(progress_target.as_ref(), &response, context_window_tokens)
                .await;
            let should_compress_after_response =
                self.should_compress_context(&response, context_window_tokens);

            // Check for cancellation immediately after LLM response
            {
                if self
                    .cancellations
                    .lock()
                    .expect("cancellations lock poisoned")
                    .contains(session_key)
                {
                    return Ok((None, messages, true, None, false, context_compressed));
                }
            }

            if response.has_tool_calls() {
                let tool_call_fingerprint = normalize_tool_call_fingerprint(&response.tool_calls);
                let fingerprint_matches =
                    last_tool_call_fingerprint.as_deref() == Some(tool_call_fingerprint.as_str());
                if fingerprint_matches {
                    repeated_tool_call_streak += 1;
                } else {
                    repeated_tool_call_streak = 1;
                    last_tool_call_fingerprint = Some(tool_call_fingerprint);
                }

                let current_tool_names: String = response
                    .tool_calls
                    .iter()
                    .map(|c| c.name.as_str())
                    .collect::<Vec<_>>()
                    .join(",");
                if !fingerprint_matches
                    && last_tool_names.as_deref() == Some(current_tool_names.as_str())
                {
                    same_tool_name_streak += 1;
                } else if !fingerprint_matches {
                    same_tool_name_streak = 1;
                    last_tool_names = Some(current_tool_names);
                }

                let openai_tool_calls = response
                    .tool_calls
                    .iter()
                    .map(|call| call.to_openai_tool_call())
                    .collect::<Vec<_>>();
                let assistant_content = response
                    .content
                    .clone()
                    .map(|text| think_re.replace_all(&text, "").trim().to_string())
                    .filter(|text| !text.is_empty());
                if let Some(content) = &assistant_content {
                    last_assistant_content = Some(content.clone());
                }
                // Reset counter if model produced content, reasoning, or tool calls
                let has_reasoning = response.reasoning_content.as_ref().is_some_and(|r| !r.trim().is_empty());
                let has_tool_calls = !response.tool_calls.is_empty();
                if assistant_content.is_some() || has_reasoning || has_tool_calls {
                    empty_output_nudge_count = 0;
                }
                self.context.add_assistant_message(
                    &mut messages,
                    assistant_content,
                    Some(openai_tool_calls),
                    response.reasoning_content.clone(),
                    response.thinking_blocks.clone(),
                );

                let tool_call_requests = response.tool_calls;

                self.send_collapse_thinking(progress_target.as_ref()).await;

                for tool_call in &tool_call_requests {
                    // Check for cancellation before each tool call
                    {
                        if self
                            .cancellations
                            .lock()
                            .expect("cancellations lock poisoned")
                            .contains(session_key)
                        {
                            return Ok((None, messages, true, None, false, context_compressed));
                        }
                    }
                    self.send_tool_hint(progress_target.as_ref(), tool_call)
                        .await;
                }

                let denied_output = self.check_approval(&tool_call_requests).await;

                if let Some(denied_outputs) = denied_output {
                    for (tool_call, output) in tool_call_requests
                        .into_iter()
                        .zip(denied_outputs.into_iter())
                    {
                        self.context.add_tool_result(
                            &mut messages,
                            &tool_call.id,
                            &tool_call.name,
                            output.into_value(),
                        );
                    }
                    final_content = Some("Task stopped: user denied the file change.".to_string());
                    break;
                } else {
                    let tools = self.tools.clone();
                    let sem = self.tool_semaphore.clone();
                    let tool_outputs = join_all(tool_call_requests.iter().map(|tool_call| {
                        let tools = tools.clone();
                        let sem = sem.clone();
                        let name = tool_call.name.clone();
                        let args = tool_call.arguments.clone();
                        async move {
                            let _permit = sem.acquire_owned().await.expect("tool semaphore closed");
                            tools.execute(&name, args).await
                        }
                    }))
                    .await;

                    for (tool_call, output) in
                        tool_call_requests.into_iter().zip(tool_outputs.into_iter())
                    {
                        self.send_tool_result(progress_target.as_ref(), &tool_call.name, &output)
                            .await;
                        self.context.add_tool_result(
                            &mut messages,
                            &tool_call.id,
                            &tool_call.name,
                            output.into_value(),
                        );
                    }
                }
                if should_compress_after_response {
                    compression_pending = true;
                }

                if let Err(err) = self.persist_session_messages(session_key, &messages) {
                    eprintln!("periodic session save failed: {err:#}");
                }

                // Break the main loop if cancellation was detected during tool execution
                {
                    if self
                        .cancellations
                        .lock()
                        .expect("cancellations lock poisoned")
                        .contains(session_key)
                    {
                        return Ok((None, messages, true, None, false, context_compressed));
                    }
                }

                if repeated_tool_call_streak >= 30 {
                    final_content = Some(build_repeated_tool_loop_message(
                        repeated_tool_call_streak,
                        &messages,
                        last_assistant_content.as_deref(),
                    ));
                    break;
                }

                if same_tool_name_streak >= 10 && repeated_tool_call_streak < 30 {
                    same_tool_nudges_sent += 1;
                    if same_tool_nudges_sent >= 2 {
                        final_content = Some(format!(
                            "I was stuck in a search loop calling {} repeatedly. \
                             Here is what I found before being stopped:\n\n{}",
                            last_tool_names.as_deref().unwrap_or("unknown"),
                            last_assistant_content
                                .as_deref()
                                .unwrap_or("(no summary produced)")
                        ));
                        break;
                    }
                    let nudge = format!(
                        "[SYSTEM] You have called the same tool ({}) {} times in a row with \
                         different arguments. You appear to be stuck in a search loop. \
                         STOP searching immediately. Synthesize what you have found so far \
                         into a final answer. Do NOT make any more tool calls. If you cannot \
                         fully answer the question with the information gathered, explain \
                         what is missing and present your partial findings.",
                        last_tool_names.as_deref().unwrap_or("unknown"),
                        same_tool_name_streak
                    );
                    messages.push(ChatMessage::text("user", nudge));
                    same_tool_name_streak = 0;
                }
            } else {
                let content = response
                    .content
                    .clone()
                    .map(|text| think_re.replace_all(&text, "").trim().to_string())
                    .filter(|text| !text.is_empty());
                if let Some(content) = &content {
                    last_assistant_content = Some(content.clone());
                }
                let has_reasoning = response
                    .reasoning_content
                    .as_ref()
                    .is_some_and(|r| !r.trim().is_empty());
                self.context.add_assistant_message(
                    &mut messages,
                    content.clone(),
                    None,
                    response.reasoning_content.clone(),
                    response.thinking_blocks.clone(),
                );

                if session_key.starts_with("cli:") {
                    let (subagent_results, running, timed_out) = self
                        .subagents
                        .wait_for_session_results(session_key, SUBAGENT_WAIT_TIMEOUT)
                        .await;
                    if !subagent_results.is_empty() || timed_out {
                        messages.push(ChatMessage::text(
                            "user",
                            format_subagent_results_for_context(
                                &subagent_results,
                                running,
                                timed_out,
                            ),
                        ));
                        if should_compress_after_response {
                            messages = self
                                .compress_context_for_next_request(
                                    messages,
                                    active_model,
                                    context_window_tokens,
                                    progress_target.as_ref(),
                                )
                                .await?;
                            context_compressed = true;
                        }
                        continue;
                    }
                }

                if content.is_none() && has_reasoning && !empty_content_nudge_sent {
                    empty_content_nudge_sent = true;
                    messages.push(ChatMessage::text(
                        "user",
                        "[SYSTEM] You produced reasoning but no visible response text. \
                         Please provide your answer or summary as regular text content.",
                    ));
                    if should_compress_after_response {
                        compression_pending = true;
                    }
                    continue;
                }

                // Nudge the model when it returns completely empty output (no content, no reasoning)
                if content.is_none() && !has_reasoning {
                    empty_output_nudge_count += 1;
                    if empty_output_nudge_count >= 3 {
                        return Err(anyhow::anyhow!(
                            "Model repeatedly produced no output after 3 attempts. \
                             The model may be malfunctioning or the request is too ambiguous. \
                             Please try rephrasing your request or switching to a different model."
                        ));
                    }
                    messages.push(ChatMessage::text(
                        "user",
                        "[SYSTEM] You erroneously emitted no output. Please complete the last \
                         directive issued by the user with a proper response.",
                    ));
                    if should_compress_after_response {
                        compression_pending = true;
                    }
                    continue;
                }

                if should_compress_after_response {
                    messages = self
                        .compress_context_for_next_request(
                            messages,
                            active_model,
                            context_window_tokens,
                            progress_target.as_ref(),
                        )
                        .await?;
                    context_compressed = true;
                }

                let steer_messages: Vec<String> = {
                    let mut rx_guard = self.steer_rx.lock().expect("steer_rx lock poisoned");
                    let mut collected = Vec::new();
                    if let Some(ref mut rx) = *rx_guard {
                        while let Ok(msg) = rx.try_recv() {
                            collected.push(msg);
                        }
                    }
                    collected
                };
                if !steer_messages.is_empty() {
                    for steer_msg in steer_messages {
                        self.send_steer_hint(progress_target.as_ref(), &steer_msg)
                            .await;
                        messages.push(ChatMessage::text("user", steer_msg));
                    }
                    continue;
                }

                final_content = content;
                final_reasoning_content = response.reasoning_content.clone();
                completed_normally = true;
                break;
            }
        }

        // Final cancellation check before returning
        {
            if self
                .cancellations
                .lock()
                .expect("cancellations lock poisoned")
                .contains(session_key)
            {
                return Ok((None, messages, true, None, false, context_compressed));
            }
        }

        if final_content.is_none() && self.max_iterations > 0 {
            final_content = Some(build_iteration_limit_message(
                self.max_iterations,
                &messages,
                last_assistant_content.as_deref(),
            ));
        }

        Ok((
            final_content,
            messages,
            false,
            final_reasoning_content,
            completed_normally,
            context_compressed,
        ))
    }

    fn should_compress_context(
        &self,
        response: &LlmResponse,
        context_window_tokens: usize,
    ) -> bool {
        let prompt_tokens = response.usage.prompt_tokens;
        prompt_tokens > 0
            && context_window_tokens > 0
            && prompt_tokens.saturating_mul(100)
                >= context_window_tokens.saturating_mul(CONTEXT_COMPRESSION_THRESHOLD_PERCENT)
    }

    async fn compress_context_for_next_request(
        &self,
        messages: Vec<ChatMessage>,
        active_model: &str,
        context_window_tokens: usize,
        target: Option<&ProgressTarget>,
    ) -> Result<Vec<ChatMessage>> {
        if messages.len() <= 3 {
            return Ok(messages);
        }

        self.send_backend_tool_hint(
            target,
            "context_compression",
            serde_json::json!({
                "task": "compress conversation context",
                "_summarizing": true,
                "thresholdPercent": CONTEXT_COMPRESSION_THRESHOLD_PERCENT,
            }),
        )
        .await;

        let summary_result = self
            .summarize_context_for_compression(&messages, active_model, context_window_tokens)
            .await;
        let summary = match summary_result {
            Ok(summary) if !summary.trim().is_empty() => summary,
            Ok(_) => build_local_context_summary(&messages),
            Err(err) => {
                eprintln!("failed to compress context with provider: {err}");
                build_local_context_summary(&messages)
            }
        };
        let compacted = rebuild_messages_with_context_summary(messages, summary);

        self.send_backend_tool_hint(
            target,
            "context_compression_done",
            serde_json::json!({
                "task": "context compression complete",
                "_summarizing_done": true,
            }),
        )
        .await;

        Ok(compacted)
    }

    async fn summarize_context_for_compression(
        &self,
        messages: &[ChatMessage],
        active_model: &str,
        context_window_tokens: usize,
    ) -> Result<String> {
        let target_tokens =
            (context_window_tokens / CONTEXT_COMPRESSION_TARGET_DIVISOR).clamp(1024, 8 * 1024);
        let prompt = build_context_compression_prompt(messages, target_tokens);
        let response = self
            .provider
            .chat_with_retry(
                &[
                    ChatMessage::text(
                        "system",
                        "You are a context compression specialist. You produce concise, \
                         structured summaries of agent conversations that preserve all \
                         actionable information while dramatically reducing token count. \
                         Preserve: file paths, function names, decisions made, errors \
                         encountered, user preferences, unfinished work, and tool results \
                         that inform next steps. Discard: verbose tool output, repeated \
                         attempts, exploratory dead ends (unless the dead end itself is \
                         important context), and raw file contents that have been acted upon.",
                    ),
                    ChatMessage::text("user", prompt),
                ],
                None,
                Some(active_model),
                Some(target_tokens),
                Some(0.1),
            )
            .await
            .context("context compression request failed")?;
        response
            .content
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty())
            .context("context compression response was empty")
    }

    fn recent_tool_diagnostics(messages: &[ChatMessage]) -> Vec<String> {
        messages
            .iter()
            .rev()
            .filter(|message| message.role == "tool")
            .filter_map(|message| {
                let name = message.name.as_deref().unwrap_or("tool");
                let text = message.content_as_text()?;
                let text = text.trim();
                if text.is_empty() {
                    return None;
                }
                Some(format!("{name}: {}", truncate_for_diagnostic(text, 220)))
            })
            .take(3)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect()
    }

    fn is_file_modifying_tool(name: &str) -> bool {
        matches!(name, "write_file" | "edit_file")
    }

    async fn check_approval(
        &self,
        tool_calls: &[crate::providers::ToolCallRequest],
    ) -> Option<Vec<ToolOutput>> {
        use crate::tools::{ApprovalDecision, ApprovalRequest};

        if *self.always_allow.lock().expect("always_allow lock") {
            return None;
        }

        let callback = self
            .approval_callback
            .lock()
            .expect("approval callback lock")
            .clone();
        let callback = callback?;

        let file_modifying: Vec<_> = tool_calls
            .iter()
            .filter(|tc| Self::is_file_modifying_tool(&tc.name))
            .collect();

        if file_modifying.is_empty() {
            return None;
        }

        for tc in &file_modifying {
            let path = tc
                .arguments
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();

            let diff_lines = match tc.name.as_str() {
                "edit_file" => {
                    let old = tc
                        .arguments
                        .get("old_text")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default();
                    let new = tc
                        .arguments
                        .get("new_text")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default();
                    crate::diff::compute_diff(old, new).lines
                }
                "write_file" => {
                    let content = tc
                        .arguments
                        .get("content")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default();
                    crate::diff::compute_write_diff(content).lines
                }
                _ => Vec::new(),
            };

            let request = ApprovalRequest {
                tool_name: tc.name.clone(),
                path,
                diff_lines,
                source: None,
            };

            let decision = callback(request).await;
            match decision {
                ApprovalDecision::AllowOnce => continue,
                ApprovalDecision::AlwaysAllow => {
                    *self.always_allow.lock().expect("always_allow lock") = true;
                    return None;
                }
                ApprovalDecision::Deny => {
                    let outputs: Vec<ToolOutput> = tool_calls
                        .iter()
                        .map(|tc| {
                            if Self::is_file_modifying_tool(&tc.name) {
                                ToolOutput::Text("Error: User denied this file change.".to_string())
                            } else {
                                ToolOutput::Text(
                                    "Error: Task cancelled by user (file change denied)."
                                        .to_string(),
                                )
                            }
                        })
                        .collect();
                    return Some(outputs);
                }
            }
        }
        None
    }

    async fn send_tool_result(
        &self,
        target: Option<&ProgressTarget>,
        tool_name: &str,
        output: &ToolOutput,
    ) {
        let Some(target) = target else { return };
        let callback = self
            .progress_sender
            .lock()
            .expect("progress callback lock poisoned")
            .clone();
        let Some(callback) = callback else { return };

        let (success, summary) = match output {
            ToolOutput::Text(text) => {
                let is_error = text.starts_with("Error");
                let all_lines: Vec<&str> = text.lines().collect();
                let preview = if all_lines.len() <= 8 {
                    all_lines
                        .iter()
                        .map(|l| truncate_chars_ellipsis(l, 100))
                        .collect::<Vec<_>>()
                        .join("\n")
                } else {
                    let head: Vec<String> = all_lines[..4]
                        .iter()
                        .map(|l| truncate_chars_ellipsis(l, 100))
                        .collect();
                    let tail: Vec<String> = all_lines[all_lines.len() - 4..]
                        .iter()
                        .map(|l| truncate_chars_ellipsis(l, 100))
                        .collect();
                    format!(
                        "{}\n  … {} lines …\n{}",
                        head.join("\n"),
                        all_lines.len() - 8,
                        tail.join("\n")
                    )
                };
                (!is_error, preview)
            }
            ToolOutput::Blocks(_) => (true, "completed".to_string()),
        };

        let mut outbound = target.outbound(String::new());
        outbound
            .metadata
            .insert("_progress".to_string(), Value::Bool(true));
        outbound
            .metadata
            .insert("_tool_result".to_string(), Value::Bool(true));
        outbound.metadata.insert(
            "_tool_name".to_string(),
            Value::String(tool_name.to_string()),
        );
        outbound
            .metadata
            .insert("_tool_success".to_string(), Value::Bool(success));
        outbound
            .metadata
            .insert("_tool_result_summary".to_string(), Value::String(summary));
        let _ = callback(outbound).await;
    }

    async fn send_collapse_thinking(&self, target: Option<&ProgressTarget>) {
        let Some(target) = target else { return };
        let callback = self
            .progress_sender
            .lock()
            .expect("progress callback lock poisoned")
            .clone();
        let Some(callback) = callback else { return };
        let mut outbound = target.outbound(String::new());
        outbound
            .metadata
            .insert("_collapse_thinking".to_string(), Value::Bool(true));
        let _ = callback(outbound).await;
    }

    async fn send_tool_hint(
        &self,
        target: Option<&ProgressTarget>,
        tool_call: &crate::providers::ToolCallRequest,
    ) {
        if matches!(tool_call.name.as_str(), "message" | "spawn") {
            return;
        }
        let Some(target) = target else {
            return;
        };
        let callback = self
            .progress_sender
            .lock()
            .expect("progress callback lock poisoned")
            .clone();
        let Some(callback) = callback else {
            return;
        };
        let mut outbound = target.outbound(format_tool_hint(tool_call));
        outbound
            .metadata
            .insert("_progress".to_string(), Value::Bool(true));
        outbound
            .metadata
            .insert("_tool_hint".to_string(), Value::Bool(true));
        outbound.metadata.insert(
            "_tool_name".to_string(),
            Value::String(tool_call.name.clone()),
        );
        outbound
            .metadata
            .insert("_tool_args".to_string(), tool_call.arguments.clone());
        let _ = callback(outbound).await;
    }

    async fn send_backend_tool_hint(
        &self,
        target: Option<&ProgressTarget>,
        name: &str,
        arguments: Value,
    ) {
        let tool_call = crate::providers::ToolCallRequest {
            id: format!("backend_{name}"),
            name: name.to_string(),
            arguments,
        };
        self.send_tool_hint(target, &tool_call).await;
    }

    async fn send_steer_hint(&self, target: Option<&ProgressTarget>, content: &str) {
        let Some(target) = target else {
            return;
        };
        let callback = self
            .progress_sender
            .lock()
            .expect("progress callback lock poisoned")
            .clone();
        let Some(callback) = callback else {
            return;
        };
        let mut outbound = target.outbound(String::new());
        outbound
            .metadata
            .insert("_progress".to_string(), Value::Bool(true));
        outbound
            .metadata
            .insert("_steer_injected".to_string(), Value::Bool(true));
        outbound.metadata.insert(
            "_steer_content".to_string(),
            Value::String(content.to_string()),
        );
        let _ = callback(outbound).await;
    }

    async fn send_context_update(
        &self,
        target: Option<&ProgressTarget>,
        response: &LlmResponse,
        context_window_tokens: usize,
    ) {
        let prompt_tokens = response.usage.prompt_tokens;
        if prompt_tokens == 0 {
            return;
        }
        let Some(target) = target else {
            return;
        };
        if target.channel != "cli" {
            return;
        }
        let callback = self
            .progress_sender
            .lock()
            .expect("progress callback lock poisoned")
            .clone();
        let Some(callback) = callback else {
            return;
        };
        let cached = response.usage.cached_prompt_tokens;
        let context = format_context_usage(prompt_tokens, context_window_tokens, cached);
        let mut outbound = target.outbound(String::new());
        outbound
            .metadata
            .insert("_context_update".to_string(), Value::Bool(true));
        outbound
            .metadata
            .insert("_context".to_string(), Value::String(context));
        outbound.metadata.insert(
            "_prompt_tokens".to_string(),
            Value::from(prompt_tokens as u64),
        );
        outbound.metadata.insert(
            "_context_window_tokens".to_string(),
            Value::from(context_window_tokens as u64),
        );
        if cached > 0 {
            outbound
                .metadata
                .insert("_cached_tokens".to_string(), Value::from(cached as u64));
        }
        let _ = callback(outbound).await;
    }

    async fn handle_stop_signal(
        &self,
        msg: &InboundMessage,
        target: &ProgressTarget,
    ) -> Result<Option<OutboundMessage>> {
        let session_key = msg.session_key();
        self.cancellations
            .lock()
            .expect("cancellations lock poisoned")
            .insert(session_key.clone());
        let cancelled = self.subagents.cancel_by_session(&session_key).await;
        let is_active = self.has_active_turn(&session_key);

        if cancelled == 0 && !is_active {
            self.clear_cancellation(&session_key);
            self.stop_notifications
                .lock()
                .expect("stop notifications lock poisoned")
                .remove(&session_key);
            return Ok(Some(target.outbound("No active task to stop.")));
        }

        if is_active {
            if self.runtime_reply_sender().is_some() {
                let completion_message = if cancelled > 0 {
                    format!("Task stopped by user. Cancelled {cancelled} background task(s).")
                } else {
                    "Task stopped by user.".to_string()
                };
                self.stop_notifications
                    .lock()
                    .expect("stop notifications lock poisoned")
                    .insert(
                        session_key,
                        StopNotification {
                            target: target.clone(),
                            completion_message,
                            cancellation_observed: false,
                        },
                    );
            }
            let content = if cancelled > 0 {
                format!("Stopping current turn and {cancelled} task(s)...")
            } else {
                "Stopping current turn...".to_string()
            };
            return Ok(Some(target.outbound(content)));
        }

        self.clear_cancellation(&session_key);
        let stopped_message = format!("Stopped {cancelled} task(s) by user request.");
        if self.runtime_reply_sender().is_some() {
            self.schedule_runtime_reply(target.clone(), stopped_message);
            return Ok(Some(
                target.outbound(format!("Stopping {cancelled} task(s)...")),
            ));
        }
        Ok(Some(
            target.outbound(format!("Stopped {cancelled} task(s).")),
        ))
    }

    async fn handle_memorize_signal(
        &self,
        target: &ProgressTarget,
        memory_input: &str,
    ) -> Result<Option<OutboundMessage>> {
        if !self.memory_enabled() {
            return Ok(Some(target.outbound(
                "Memory is disabled in this mode. Run mode is required to write long-term memory.",
            )));
        }
        if memory_input.trim().is_empty() {
            return Ok(Some(
                target.outbound("Usage: /memorize <durable information to remember>"),
            ));
        }
        let entry = self
            .build_memory_entry_with_skill(
                MemoryEntryKind::UserInstructed,
                memory_input,
                Some(memory_input),
            )
            .await
            .unwrap_or_else(|err| {
                eprintln!("failed to summarize user memory entry with skill: {err}");
                build_memory_entry(
                    MemoryEntryKind::UserInstructed,
                    memory_input,
                    Some(memory_input),
                )
            });
        self.memory.store().append_memory_entry(&entry)?;
        Ok(Some(target.outbound(format!(
            "Memorized into permanent memory: {}",
            entry.title
        ))))
    }

    fn runtime_reply_sender(&self) -> Option<MessageSendCallback> {
        self.progress_sender
            .lock()
            .expect("progress callback lock poisoned")
            .clone()
    }

    fn has_active_turn(&self, session_key: &str) -> bool {
        self.active_turns
            .lock()
            .expect("active turns lock poisoned")
            .get(session_key)
            .copied()
            .unwrap_or(0)
            > 0
    }

    fn is_cancellation_pending(&self, session_key: &str) -> bool {
        self.cancellations
            .lock()
            .expect("cancellations lock poisoned")
            .contains(session_key)
    }

    fn clear_cancellation(&self, session_key: &str) -> bool {
        self.cancellations
            .lock()
            .expect("cancellations lock poisoned")
            .remove(session_key)
    }

    fn schedule_runtime_reply(&self, target: ProgressTarget, content: String) {
        let Some(callback) = self.runtime_reply_sender() else {
            return;
        };
        tokio::spawn(async move {
            tokio::task::yield_now().await;
            let outbound = target.outbound(content);
            if let Err(err) = callback(outbound).await {
                eprintln!("failed to send runtime reply: {err}");
            }
        });
    }

    async fn send_runtime_reply(&self, target: &ProgressTarget, content: String) {
        let Some(callback) = self.runtime_reply_sender() else {
            return;
        };
        let outbound = target.outbound(content);
        if let Err(err) = callback(outbound).await {
            eprintln!("failed to send runtime reply: {err}");
        }
    }

    async fn finalize_stop_state(
        &self,
        session_key: &str,
        interrupted: bool,
        failure_message: Option<String>,
    ) {
        if interrupted {
            if let Some(notification) = self
                .stop_notifications
                .lock()
                .expect("stop notifications lock poisoned")
                .get_mut(session_key)
            {
                notification.cancellation_observed = true;
            }
        }

        if self.has_active_turn(session_key) {
            return;
        }

        let had_cancellation = self.clear_cancellation(session_key);
        let notification = self
            .stop_notifications
            .lock()
            .expect("stop notifications lock poisoned")
            .remove(session_key);

        let Some(notification) = notification else {
            return;
        };

        if interrupted || notification.cancellation_observed {
            self.send_runtime_reply(&notification.target, notification.completion_message)
                .await;
            return;
        }

        if had_cancellation {
            let message = failure_message.unwrap_or_else(|| "Unable to stop task.".to_string());
            self.send_runtime_reply(&notification.target, message).await;
        }
    }

    async fn record_completed_task_memory(
        &self,
        task_text: &str,
        final_content: Option<&str>,
        messages: &[ChatMessage],
        target: Option<&ProgressTarget>,
    ) {
        if !self.memory_enabled() {
            return;
        }
        let summary_source = final_content
            .map(ToOwned::to_owned)
            .or_else(|| latest_assistant_text(messages))
            .unwrap_or_else(|| task_text.to_string());
        self.send_backend_tool_hint(
            target,
            "memory_summary",
            serde_json::json!({"task":"summarize completed task memory","_summarizing":true}),
        )
        .await;
        let entry = self
            .build_memory_entry_with_skill(
                MemoryEntryKind::TaskSummary,
                task_text,
                Some(&summary_source),
            )
            .await
            .unwrap_or_else(|err| {
                eprintln!("failed to summarize task memory entry with skill: {err}");
                build_memory_entry(
                    MemoryEntryKind::TaskSummary,
                    task_text,
                    Some(&summary_source),
                )
            });
        if let Err(err) = self.memory.store().append_memory_entry(&entry) {
            eprintln!("failed to append task summary to memory: {err}");
        }
        self.send_backend_tool_hint(
            target,
            "memory_summary_done",
            serde_json::json!({"task":"memory summary complete","_summarizing_done":true}),
        )
        .await;
    }

    async fn build_memory_entry_with_skill(
        &self,
        kind: MemoryEntryKind,
        task_text: &str,
        summary_source: Option<&str>,
    ) -> Result<MemoryEntry> {
        let source = summary_source.unwrap_or(task_text);
        let skill = SkillsLoader::new(&self.workspace, None)
            .load_skills_for_context(&["memory-entry-writer".to_string()]);
        let system_prompt = if skill.trim().is_empty() {
            default_memory_entry_writer_prompt().to_string()
        } else {
            format!(
                "{skill}\n\nReturn only valid JSON with keys `title`, `summary`, and `attention_points`."
            )
        };
        let user_prompt = format!(
            "Memory entry type: {}\n\nPrimary input:\n{}\n\nSource material to summarize:\n{}",
            memory_entry_kind_label(kind),
            task_text.trim(),
            source.trim()
        );
        let response = self
            .provider
            .chat_with_retry(
                &[
                    ChatMessage::text("system", system_prompt),
                    ChatMessage::text("user", user_prompt),
                ],
                None,
                Some(&self.model),
                None,
                Some(0.1),
            )
            .await
            .context("memory summary request failed")?;
        let content = response
            .content
            .filter(|text| !text.trim().is_empty())
            .context("memory summary response was empty")?;
        let parsed = parse_memory_entry_response(&content)?;
        Ok(MemoryEntry {
            kind,
            title: sanitize_memory_title(&parsed.title, task_text),
            summary: sanitize_memory_summary(&parsed.summary, source),
            attention_points: sanitize_attention_points(parsed.attention_points),
            recorded_at: crate::util::now_iso(),
        })
    }

    fn status_response(&self, msg: &InboundMessage, session: &Session) -> OutboundMessage {
        OutboundMessage {
            channel: msg.channel.clone(),
            chat_id: msg.chat_id.clone(),
            content: self.build_status_content(session),
            reply_to: None,
            media: Vec::new(),
            reasoning_content: None,
            metadata: msg.metadata.clone(),
        }
    }

    fn build_status_content(&self, session: &Session) -> String {
        let (prompt_tokens, completion_tokens, cached_tokens) =
            *self.last_usage.lock().expect("usage lock poisoned");
        let latest_context_tokens = *self
            .last_context_prompt_tokens
            .lock()
            .expect("context prompt lock poisoned");
        let context_tokens = if latest_context_tokens > 0 {
            latest_context_tokens
        } else {
            session.context_tokens().unwrap_or(0)
        };
        let active_model = self.session_model(session);
        let context_window_tokens = self.session_context_window_tokens(session);
        build_status_content(
            env!("CARGO_PKG_VERSION"),
            &active_model,
            &self.workspace.display().to_string(),
            self.start_time.elapsed().as_secs(),
            prompt_tokens,
            completion_tokens,
            cached_tokens,
            context_window_tokens,
            session.get_history(0).len(),
            context_tokens,
        )
    }

    fn format_backend_session_notice(&self, session: &Session) -> String {
        format!(
            "{}\n\n{}",
            format_backend_session_notice(session.get_history(0).len()),
            italicize_markdown_lines(&self.build_status_content(session))
        )
    }

    fn record_usage(&self, response: &LlmResponse) {
        let mut usage = self.last_usage.lock().expect("usage lock poisoned");
        usage.0 += response.usage.prompt_tokens;
        usage.1 += response.usage.completion_tokens;
        usage.2 += response.usage.cached_prompt_tokens;
        if response.usage.prompt_tokens > 0 {
            *self
                .last_context_prompt_tokens
                .lock()
                .expect("context prompt lock poisoned") = response.usage.prompt_tokens;
        }
    }

    fn apply_latest_context_usage_metadata(&self, session: &mut Session) {
        let latest_context_tokens = *self
            .last_context_prompt_tokens
            .lock()
            .expect("context prompt lock poisoned");
        session.set_context_tokens(latest_context_tokens);
    }

    fn save_turn(&self, session: &mut Session, messages: &[ChatMessage]) -> Result<()> {
        let skip = 1 + session.get_history(0).len();
        for message in messages.iter().skip(skip) {
            if let Some(stored) = self.prepare_message_for_session_storage(message) {
                session.messages.push(stored);
            }
        }
        session.updated_at = crate::util::now_iso();
        Ok(())
    }

    fn replace_session_messages(&self, session: &mut Session, messages: &[ChatMessage]) {
        session.messages.clear();
        session.last_consolidated = 0;
        for message in messages.iter().skip(1) {
            if let Some(stored) = self.prepare_message_for_session_storage(message) {
                session.messages.push(stored);
            }
        }
        session.updated_at = crate::util::now_iso();
    }

    fn prepare_message_for_session_storage(&self, message: &ChatMessage) -> Option<ChatMessage> {
        let has_reasoning = message
            .reasoning_content
            .as_ref()
            .is_some_and(|r| !r.trim().is_empty());
        let has_thinking = message
            .thinking_blocks
            .as_ref()
            .is_some_and(|b| !b.is_empty());
        if message.role == "assistant"
            && message.content.is_none()
            && message.tool_calls.as_ref().is_none_or(Vec::is_empty)
            && !has_reasoning
            && !has_thinking
        {
            return None;
        }
        let mut stored = message.clone();
        if stored.timestamp.is_none() {
            stored.timestamp = Some(crate::util::now_iso());
        }
        let stored = sanitize_message_for_storage(stored)?;
        if let Some(Value::String(text)) = &stored.content
            && text.trim().is_empty()
        {
            return None;
        }
        Some(stored)
    }

    fn persist_session_messages(&self, session_key: &str, messages: &[ChatMessage]) -> Result<()> {
        let mut sessions = self.sessions.lock().expect("session manager lock poisoned");
        let mut session = sessions.get_or_create(session_key)?;
        self.save_turn(&mut session, messages)?;
        self.apply_latest_context_usage_metadata(&mut session);
        let context_window_tokens = self.session_context_window_tokens(&session);
        self.maybe_consolidate_session(&mut session, context_window_tokens)?;
        sessions.save(&session)?;
        Ok(())
    }

    #[allow(dead_code)]
    pub(crate) async fn consolidate_session_async(&self, session_key: &str) {
        if !self.memory_enabled() {
            return;
        }
        let (mut session, context_window_tokens) = {
            let mut sessions = self.sessions.lock().expect("session manager lock poisoned");
            match sessions.get_or_create(session_key) {
                Ok(session) => {
                    let cwt = self.session_context_window_tokens(&session);
                    (session, cwt)
                }
                Err(_) => return,
            }
        };
        let model = self.session_model(&session);
        let result = self
            .memory
            .maybe_consolidate_by_tokens_with_provider(
                &mut session,
                context_window_tokens,
                self.provider.as_ref(),
                &model,
            )
            .await;
        if result.is_ok() {
            let mut sessions = self.sessions.lock().expect("session manager lock poisoned");
            let _ = sessions.save(&session);
        }
    }

    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    pub async fn session_status_content(&self, session_key: &str) -> Result<String> {
        self.refresh_session_model_metadata(session_key).await?;
        let mut sessions = self.sessions.lock().expect("session manager lock poisoned");
        let session = sessions.get_or_create(session_key)?;
        Ok(self.build_status_content(&session))
    }

    pub fn snapshot(&self) -> Result<AgentSnapshot> {
        let sessions = self.sessions.lock().expect("session manager lock poisoned");
        let session_count = sessions.list_session_summaries()?.len();
        let (last_prompt_tokens, last_completion_tokens, last_cached_tokens) =
            *self.last_usage.lock().expect("usage lock poisoned");
        Ok(AgentSnapshot {
            model: self.model.clone(),
            workspace: self.workspace.display().to_string(),
            uptime_seconds: self.start_time.elapsed().as_secs(),
            max_iterations: self.max_iterations,
            context_window_tokens: self.context_window_tokens,
            session_count,
            running_subagents: self.subagents.get_running_count(),
            last_prompt_tokens,
            last_completion_tokens,
            last_cached_tokens,
        })
    }

    pub fn session_summaries(&self) -> Result<Vec<crate::storage::SessionSummary>> {
        let sessions = self.sessions.lock().expect("session manager lock poisoned");
        sessions.list_session_summaries()
    }

    pub fn delete_session(&self, key: &str) -> Result<bool> {
        let mut sessions = self.sessions.lock().expect("session manager lock poisoned");
        sessions.delete(key)
    }

    pub fn tool_output_to_string(output: ToolOutput) -> Result<String> {
        match output {
            ToolOutput::Text(text) => Ok(text),
            ToolOutput::Blocks(blocks) => Ok(serde_json::to_string(&blocks)?),
        }
    }
}

struct ActiveTurnGuard {
    set: Arc<Mutex<BTreeMap<String, usize>>>,
    key: String,
}

impl ActiveTurnGuard {
    fn new(set: Arc<Mutex<BTreeMap<String, usize>>>, key: String) -> Self {
        {
            let mut counts = set.lock().unwrap();
            *counts.entry(key.clone()).or_insert(0) += 1;
        }
        Self { set, key }
    }
}

impl Drop for ActiveTurnGuard {
    fn drop(&mut self) {
        let mut set = self.set.lock().unwrap();
        if let Some(count) = set.get_mut(&self.key) {
            *count -= 1;
            if *count == 0 {
                set.remove(&self.key);
            }
        }
    }
}

#[derive(Clone)]
struct StopNotification {
    target: ProgressTarget,
    completion_message: String,
    cancellation_observed: bool,
}

struct SessionSetup {
    response: Option<OutboundMessage>,
    session_notice: Option<String>,
    active_model: String,
    context_window_tokens: usize,
}

#[derive(Clone)]
struct ProgressTarget {
    channel: String,
    chat_id: String,
    session_key: String,
    metadata: BTreeMap<String, Value>,
}

impl ProgressTarget {
    fn from_inbound(msg: &InboundMessage) -> Self {
        Self {
            channel: msg.channel.clone(),
            chat_id: msg.chat_id.clone(),
            session_key: msg.session_key(),
            metadata: msg.metadata.clone(),
        }
    }

    fn outbound(&self, content: impl Into<String>) -> OutboundMessage {
        let mut metadata = self.metadata.clone();
        metadata.insert(
            "_session_key".to_string(),
            Value::String(self.session_key.clone()),
        );
        OutboundMessage {
            channel: self.channel.clone(),
            chat_id: self.chat_id.clone(),
            content: content.into(),
            reply_to: None,
            media: Vec::new(),
            reasoning_content: None,
            metadata,
        }
    }
}

const SESSION_MODEL_KEY: &str = "model";
const SESSION_CONTEXT_WINDOW_KEY: &str = "contextWindowTokens";

fn should_announce_backend_session(msg: &InboundMessage, trimmed: &str) -> bool {
    msg.channel != "cli" && special_command_action(trimmed).is_none()
}

fn format_backend_session_notice(session_message_count: usize) -> String {
    if session_message_count == 0 {
        "Session: started new session for this conversation.".to_string()
    } else {
        let label = if session_message_count == 1 {
            "message"
        } else {
            "messages"
        };
        format!("Session: resuming {session_message_count} previous {label}; /new to start fresh.")
    }
}

fn italicize_markdown_lines(content: &str) -> String {
    content
        .lines()
        .map(|line| {
            if line.trim().is_empty() {
                String::new()
            } else {
                format!("_{line}_")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn parse_model_command(trimmed: &str) -> Option<Option<String>> {
    for prefix in ["/model", "model"] {
        if trimmed.eq_ignore_ascii_case(prefix) {
            return Some(None);
        }
        if trimmed.len() > prefix.len()
            && trimmed[..prefix.len()].eq_ignore_ascii_case(prefix)
            && trimmed[prefix.len()..].starts_with(char::is_whitespace)
        {
            return Some(Some(trimmed[prefix.len()..].trim().to_string()));
        }
    }
    None
}

fn find_model_context_window_tokens(models: &[ProviderModelInfo], model: &str) -> Option<usize> {
    resolve_runtime_model_info(models, model).and_then(|item| item.context_window_tokens)
}

fn resolve_runtime_model_info<'a>(
    models: &'a [ProviderModelInfo],
    model: &str,
) -> Option<&'a ProviderModelInfo> {
    models
        .iter()
        .find(|item| item.id == model)
        .or_else(|| {
            let requested = model.to_ascii_lowercase();
            let mut matches = models
                .iter()
                .filter(|item| item.id.to_ascii_lowercase() == requested);
            let first = matches.next()?;
            matches.next().is_none().then_some(first)
        })
        .or_else(|| {
            let basename = model_basename(model)?;
            let mut matches = models.iter().filter(|item| {
                model_basename(&item.id)
                    .map(|name| name.eq_ignore_ascii_case(basename))
                    .unwrap_or(false)
            });
            let first = matches.next()?;
            matches.next().is_none().then_some(first)
        })
        .or_else(|| (models.len() == 1).then_some(&models[0]))
}

fn resolve_model_selection<'a>(
    models: &'a [ProviderModelInfo],
    requested_model: &str,
) -> Option<&'a ProviderModelInfo> {
    models
        .iter()
        .find(|item| item.id == requested_model)
        .or_else(|| {
            let requested = requested_model.to_ascii_lowercase();
            let mut matches = models
                .iter()
                .filter(|item| item.id.to_ascii_lowercase() == requested);
            let first = matches.next()?;
            matches.next().is_none().then_some(first)
        })
        .or_else(|| {
            let basename = model_basename(requested_model)?;
            let mut matches = models.iter().filter(|item| {
                model_basename(&item.id)
                    .map(|name| name.eq_ignore_ascii_case(basename))
                    .unwrap_or(false)
            });
            let first = matches.next()?;
            matches.next().is_none().then_some(first)
        })
}

fn model_basename(model: &str) -> Option<&str> {
    let trimmed = model.trim_end_matches(['/', '\\']);
    Path::new(trimmed)
        .file_name()
        .and_then(|name| name.to_str())
}

fn special_command_action(trimmed: &str) -> Option<&'static str> {
    if parse_model_command(trimmed).is_some() {
        return Some("list or switch models");
    }
    match trimmed {
        "/new" | "new" | "/clear" | "clear" | "[clear]" => Some("start a new session"),
        "/status" | "status" => Some("get status"),
        "/help" | "help" => Some("show help"),
        _ => None,
    }
}

fn parse_memorize_command(content: &str) -> Option<String> {
    let trimmed = content.trim();
    for prefix in ["/memorize", "memorize", "[memorize]"] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            if rest.is_empty() || rest.starts_with(char::is_whitespace) {
                return Some(rest.trim().to_string());
            }
        }
    }
    None
}

fn build_memory_entry(
    kind: MemoryEntryKind,
    task_text: &str,
    summary_source: Option<&str>,
) -> MemoryEntry {
    let title = summarize_title(task_text);
    let summary = summarize_body(summary_source.unwrap_or(task_text));
    let attention_points = extract_attention_points(summary_source.unwrap_or(task_text));
    MemoryEntry {
        kind,
        title,
        summary,
        attention_points,
        recorded_at: crate::util::now_iso(),
    }
}

#[derive(Debug, Deserialize)]
struct MemoryEntrySummary {
    title: String,
    summary: String,
    #[serde(default)]
    attention_points: Vec<String>,
}

fn summarize_title(text: &str) -> String {
    let candidate = text
        .split(['\n', '.', '!', '?'])
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("Task");
    truncate_plain(candidate, 80)
}

fn summarize_body(text: &str) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let normalized = if collapsed.is_empty() {
        "No summary recorded.".to_string()
    } else {
        collapsed
    };
    truncate_plain(&normalized, 320)
}

fn sanitize_memory_title(title: &str, fallback: &str) -> String {
    let collapsed = collapse_plain_text(title);
    if collapsed.is_empty() {
        summarize_title(fallback)
    } else {
        truncate_plain(&collapsed, 80)
    }
}

fn sanitize_memory_summary(summary: &str, fallback: &str) -> String {
    let collapsed = collapse_plain_text(summary);
    if collapsed.is_empty() {
        summarize_body(fallback)
    } else {
        truncate_plain(&collapsed, 240)
    }
}

fn sanitize_attention_points(points: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut normalized = Vec::new();
    for point in points {
        let collapsed = collapse_plain_text(&point);
        if collapsed.is_empty() {
            continue;
        }
        let shortened = truncate_plain(&collapsed, 140);
        let key = shortened.to_ascii_lowercase();
        if seen.insert(key) {
            normalized.push(shortened);
        }
        if normalized.len() >= 5 {
            break;
        }
    }
    normalized
}

fn extract_attention_points(text: &str) -> Vec<String> {
    let mut points = text
        .lines()
        .map(str::trim)
        .filter(|line| {
            line.starts_with("- ")
                || line.starts_with("* ")
                || line
                    .chars()
                    .next()
                    .is_some_and(|ch| ch.is_ascii_digit() && line.contains('.'))
                || line.to_ascii_lowercase().contains("attention")
                || line.to_ascii_lowercase().contains("warning")
                || line.to_ascii_lowercase().contains("follow up")
                || line.to_ascii_lowercase().contains("next step")
        })
        .map(|line| {
            line.trim_start_matches(|ch: char| {
                ch == '-' || ch == '*' || ch.is_ascii_digit() || ch == '.' || ch == ' '
            })
            .trim()
            .to_string()
        })
        .filter(|line| !line.is_empty())
        .take(5)
        .collect::<Vec<_>>();
    points.sort();
    points.dedup();
    points
}

fn parse_memory_entry_response(content: &str) -> Result<MemoryEntrySummary> {
    // Strip reasoning tags that some providers include in content
    let think_re = Regex::new(r"(?s)<think>.*?</think>").expect("valid think regex");
    let cleaned = think_re.replace_all(content, "").trim().to_string();
    for candidate in extract_json_candidates(&cleaned) {
        if let Ok(parsed) = serde_json::from_str::<MemoryEntrySummary>(&candidate) {
            return Ok(parsed);
        }
    }
    Err(anyhow::anyhow!(
        "memory summary response was not valid JSON: {}",
        truncate_plain(&cleaned, 160)
    ))
}

fn extract_json_candidates(content: &str) -> Vec<String> {
    let trimmed = content.trim();
    let mut candidates = Vec::new();
    if !trimmed.is_empty() {
        candidates.push(trimmed.to_string());
    }
    if let Some(stripped) = strip_code_fence(trimmed) {
        candidates.push(stripped);
    }
    if let (Some(start), Some(end)) = (trimmed.find('{'), trimmed.rfind('}')) {
        if end >= start {
            candidates.push(trimmed[start..=end].to_string());
        }
    }
    candidates.sort();
    candidates.dedup();
    candidates
}

fn strip_code_fence(content: &str) -> Option<String> {
    let trimmed = content.trim();
    if !trimmed.starts_with("```") || !trimmed.ends_with("```") {
        return None;
    }
    let body = trimmed
        .trim_start_matches("```json")
        .trim_start_matches("```JSON")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    (!body.is_empty()).then_some(body.to_string())
}

fn latest_assistant_text(messages: &[ChatMessage]) -> Option<String> {
    messages
        .iter()
        .rev()
        .find(|message| message.role == "assistant")
        .and_then(ChatMessage::content_as_text)
}

fn format_subagent_results_for_context(
    results: &[CompletedSubagentResult],
    running: usize,
    timed_out: bool,
) -> String {
    let mut text = String::from(
        "[Subagent results injected into the main task context]\n\nUse these results to continue the original task.",
    );
    if results.is_empty() {
        text.push_str("\n\nNo completed subagent results were available.");
    }
    for result in results {
        text.push_str(&format!(
            "\n\n[Subagent '{}' completed]\nTask ID: {}\nTask: {}\nResult:\n{}",
            result.label, result.task_id, result.task, result.result
        ));
    }
    if timed_out {
        text.push_str(&format!(
            "\n\nTimed out waiting for subagents; {running} subagent(s) still running."
        ));
    }
    text
}

fn format_context_usage(
    context_tokens: usize,
    context_window_tokens: usize,
    cached_tokens: usize,
) -> String {
    let pct = if context_window_tokens > 0 {
        (context_tokens * 100) / context_window_tokens
    } else {
        0
    };
    if cached_tokens > 0 && context_tokens > 0 {
        let cache_pct = (cached_tokens * 100) / context_tokens;
        format!("{context_tokens}/{context_window_tokens} ({pct}%, {cache_pct}% cached)")
    } else {
        format!("{context_tokens}/{context_window_tokens} ({pct}%)")
    }
}

fn build_context_compression_prompt(messages: &[ChatMessage], target_tokens: usize) -> String {
    let latest_user = latest_user_message_text(messages).unwrap_or_default();
    let transcript = build_compression_transcript(messages, target_tokens.saturating_mul(30));
    format!(
        "Compress the conversation below to ~{target_tokens} tokens (about \
1/{CONTEXT_COMPRESSION_TARGET_DIVISOR} of original size).\n\n\
## What to preserve (in order of priority)\n\
1. **Decisions and outcomes** — what was decided, what worked, what failed\n\
2. **File paths and locations** — exact paths, line numbers, function names referenced\n\
3. **User preferences and constraints** — explicit requirements and style choices\n\
4. **Unfinished work** — tasks in progress, next steps planned\n\
5. **Errors and blockers** — issues encountered that may recur\n\
6. **Key tool results** — findings from grep/search/read that inform next steps\n\n\
## What to discard\n\
- Verbose raw file contents (keep only the relevant excerpts)\n\
- Repeated/failed tool attempts (keep only the final working approach)\n\
- Exploratory dead ends (unless the dead end itself is important)\n\
- Assistant reasoning/preamble that restates the user's request\n\n\
## Format\n\
Return concise markdown bullets grouped by topic. Use `code formatting` for paths and \
identifiers. The current user request is kept separately and must NOT be included.\n\n\
Current user request (kept separately):\n{}\n\n\
Conversation to compress:\n{}",
        truncate_plain(&latest_user, 4_000),
        transcript
    )
}

fn build_compression_transcript(messages: &[ChatMessage], max_chars: usize) -> String {
    let latest_user_idx = latest_user_message_index(messages);
    let mut entries = Vec::new();
    let mut used = 0_usize;
    for (idx, message) in messages.iter().enumerate().rev() {
        if idx == 0 || Some(idx) == latest_user_idx {
            continue;
        }
        let Some(text) = message.content_as_text() else {
            continue;
        };
        let text = collapse_plain_text(&text);
        if text.is_empty() {
            continue;
        }
        let entry = format!("{}: {}", message.role, truncate_plain(&text, 2_000));
        let entry_len = entry.len() + 1;
        if used + entry_len > max_chars && !entries.is_empty() {
            break;
        }
        used += entry_len;
        entries.push(entry);
    }
    entries.reverse();
    if entries.is_empty() {
        "No prior context available.".to_string()
    } else {
        entries.join("\n")
    }
}

fn rebuild_messages_with_context_summary(
    messages: Vec<ChatMessage>,
    summary: String,
) -> Vec<ChatMessage> {
    if messages.is_empty() {
        return messages;
    }
    let latest_user_idx = latest_user_message_index(&messages);
    let mut compacted = vec![messages[0].clone()];
    compacted.push(ChatMessage::text(
        "assistant",
        format!("[Compressed Context]\n{}", summary.trim()),
    ));
    if let Some(idx) = latest_user_idx {
        compacted.push(messages[idx].clone());
        if let Some(assistant) = latest_plain_assistant_after(&messages, idx) {
            compacted.push(assistant);
        }
    }
    compacted
}

fn build_local_context_summary(messages: &[ChatMessage]) -> String {
    let transcript = build_compression_transcript(messages, 16_000);
    format!(
        "Provider context compression failed; retained an extractive compressed context.\n\n{}",
        transcript
    )
}

fn latest_user_message_index(messages: &[ChatMessage]) -> Option<usize> {
    messages
        .iter()
        .enumerate()
        .rev()
        .find(|(_, message)| message.role == "user")
        .map(|(idx, _)| idx)
}

fn latest_user_message_text(messages: &[ChatMessage]) -> Option<String> {
    latest_user_message_index(messages).and_then(|idx| messages[idx].content_as_text())
}

fn latest_plain_assistant_after(messages: &[ChatMessage], start: usize) -> Option<ChatMessage> {
    messages
        .iter()
        .skip(start + 1)
        .rev()
        .find(|message| {
            message.role == "assistant"
                && message.content.is_some()
                && message.tool_calls.as_ref().is_none_or(Vec::is_empty)
        })
        .cloned()
}

fn truncate_plain(text: &str, max_chars: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= max_chars {
        return trimmed.to_string();
    }
    let mut shortened = trimmed.chars().take(max_chars).collect::<String>();
    shortened.push_str("...");
    shortened
}

fn collapse_plain_text(text: &str) -> String {
    text.replace('\n', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn memory_entry_kind_label(kind: MemoryEntryKind) -> &'static str {
    match kind {
        MemoryEntryKind::TaskSummary => "Task Summary",
        MemoryEntryKind::UserInstructed => "User Instructed Memory",
        MemoryEntryKind::ConsolidationSummary => "Consolidation Summary",
    }
}

fn default_memory_entry_writer_prompt() -> &'static str {
    "You write concise durable memory entries for MEMORY.md.\n\
\n\
Summarize the provided material into a compact JSON object for long-term memory.\n\
\n\
Rules:\n\
- Title: plain text, short, specific, under 80 characters.\n\
- Summary: plain text, 1-2 short sentences, under 240 characters.\n\
- attention_points: array of short plain-text bullets. Include only durable cautions, follow-ups, or constraints. Use an empty array when there is nothing important.\n\
- Do not copy raw markdown sections, code blocks, URLs, transcripts, or large excerpts.\n\
- Prefer durable facts over narration.\n\
\n\
Return only JSON with this shape:\n\
{\"title\":\"...\",\"summary\":\"...\",\"attention_points\":[\"...\"]}"
}

fn normalize_tool_call_fingerprint(tool_calls: &[crate::providers::ToolCallRequest]) -> String {
    let normalized = tool_calls
        .iter()
        .map(|call| {
            serde_json::json!({
                "name": call.name,
                "arguments": call.arguments,
            })
        })
        .collect::<Vec<_>>();
    serde_json::to_string(&normalized).unwrap_or_else(|_| format!("{normalized:?}"))
}

fn format_tool_hint(tool_call: &crate::providers::ToolCallRequest) -> String {
    let emoji = crate::util::tool_emoji(&tool_call.name);
    let preview = summarize_tool_arguments(&tool_call.arguments);
    if preview.is_empty() {
        format!("[ {emoji} {} ]", tool_call.name)
    } else {
        format!("[ {emoji} {}  {} ]", tool_call.name, preview)
    }
}

fn summarize_tool_arguments(arguments: &Value) -> String {
    match arguments {
        Value::Object(map) => {
            let preferred_keys = [
                "path",
                "target_file",
                "file",
                "command",
                "cmd",
                "url",
                "query",
                "pattern",
                "task",
                "label",
            ];
            let mut parts = Vec::new();
            let mut seen = std::collections::BTreeSet::new();
            for key in preferred_keys {
                let Some(value) = map.get(key) else {
                    continue;
                };
                seen.insert(key.to_string());
                let summary = summarize_tool_argument_value(value);
                if !summary.is_empty() {
                    parts.push(format!("{key}={summary}"));
                }
            }
            for (key, value) in map.iter() {
                if parts.len() >= 5 {
                    break;
                }
                if seen.contains(key) {
                    continue;
                }
                let summary = summarize_tool_argument_value(value);
                if !summary.is_empty() {
                    parts.push(format!("{key}={summary}"));
                }
            }
            if map.len() > parts.len() {
                parts.push("...".to_string());
            }
            if parts.is_empty() {
                String::new()
            } else {
                parts.join(" · ")
            }
        }
        Value::Null => String::new(),
        other => truncate_for_diagnostic(&other.to_string(), 72),
    }
}

fn summarize_tool_argument_value(value: &Value) -> String {
    match value {
        Value::String(text) => truncate_for_diagnostic(text, 64),
        Value::Array(items) => format!(
            "[{} item{}]",
            items.len(),
            if items.len() == 1 { "" } else { "s" }
        ),
        Value::Object(_) => "{...}".to_string(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => value.to_string(),
        Value::Null => String::new(),
    }
}

fn build_iteration_limit_message(
    max_iterations: usize,
    messages: &[ChatMessage],
    last_assistant_content: Option<&str>,
) -> String {
    let mut lines = vec![format!(
        "Stopped after reaching the tool-call limit ({max_iterations}) before the task completed."
    )];
    if let Some(content) = last_assistant_content.filter(|content| !content.trim().is_empty()) {
        lines.push(format!(
            "Last assistant intent: {}",
            truncate_for_diagnostic(content.trim(), 220)
        ));
    }
    let tool_diagnostics = AgentLoop::recent_tool_diagnostics(messages);
    if !tool_diagnostics.is_empty() {
        lines.push("Recent tool results:".to_string());
        lines.extend(tool_diagnostics.into_iter().map(|line| format!("- {line}")));
    }
    lines.push(
        "If this task legitimately needs more steps, increase `agents.defaults.maxToolIterations`."
            .to_string(),
    );
    lines.join("\n")
}

fn build_repeated_tool_loop_message(
    repeated_batches: usize,
    messages: &[ChatMessage],
    last_assistant_content: Option<&str>,
) -> String {
    let mut lines = vec![format!(
        "**Stopped because the same tool-call pattern repeated {repeated_batches} times without reaching a final answer.**"
    )];
    if let Some(content) = last_assistant_content.filter(|content| !content.trim().is_empty()) {
        lines.push(format!(
            "Last assistant intent: {}",
            truncate_for_diagnostic(content.trim(), 220)
        ));
    }
    let tool_diagnostics = AgentLoop::recent_tool_diagnostics(messages);
    if !tool_diagnostics.is_empty() {
        lines.push("Recent tool results:".to_string());
        lines.extend(tool_diagnostics.into_iter().map(|line| format!("- {line}")));
    }
    lines.push(
        "The agent appears stuck. Adjust the task, inspect the tool errors above, or raise `agents.defaults.maxToolIterations` if the workflow is valid but long."
            .to_string(),
    );
    lines.join("\n")
}

fn truncate_for_diagnostic(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let truncated = text.chars().take(max_chars).collect::<String>();
    format!("{}...", truncated.trim_end())
}

/// Trim trailing messages that form an incomplete tool-call / tool-result
/// group. An assistant message with `tool_calls` is only valid when followed
/// by a `tool` result for every declared call id. If the tail is incomplete
/// we remove the entire dangling group (the assistant + any partial tool
/// results that belong to it).
pub(crate) fn trim_unpaired_tool_tail(messages: &mut Vec<ChatMessage>) {
    loop {
        if messages.is_empty() {
            return;
        }
        // Find the last assistant message that has tool_calls.
        let last_tc_pos = messages.iter().rposition(|m| {
            m.role == "assistant" && m.tool_calls.as_ref().is_some_and(|tcs| !tcs.is_empty())
        });
        let Some(pos) = last_tc_pos else {
            return;
        };
        let declared_ids: HashSet<&str> = messages[pos]
            .tool_calls
            .as_ref()
            .unwrap()
            .iter()
            .filter_map(|tc| tc.get("id").and_then(Value::as_str))
            .collect();
        let answered_ids: HashSet<&str> = messages[pos + 1..]
            .iter()
            .filter(|m| m.role == "tool")
            .filter_map(|m| m.tool_call_id.as_deref())
            .collect();
        if declared_ids.is_subset(&answered_ids) {
            return;
        }
        messages.truncate(pos);
        continue;
    }
}

fn sanitize_message_for_storage(mut message: ChatMessage) -> Option<ChatMessage> {
    match &mut message.content {
        Some(Value::String(text)) => {
            if text.contains(ContextBuilder::RUNTIME_CONTEXT_TAG) {
                if message.role == "user" {
                    *text = strip_runtime_context_text(text);
                } else {
                    return None;
                }
            }
        }
        Some(Value::Array(blocks)) => {
            if message.role == "user" {
                blocks.retain(|block| {
                    !block
                        .get("text")
                        .and_then(Value::as_str)
                        .is_some_and(|text| text.contains(ContextBuilder::RUNTIME_CONTEXT_TAG))
                });
            }
            if blocks.is_empty() {
                return None;
            }
        }
        _ => {}
    }
    Some(message)
}

fn strip_runtime_context_text(text: &str) -> String {
    let Some(start) = text.find(ContextBuilder::RUNTIME_CONTEXT_TAG) else {
        return text.to_string();
    };
    let before = text[..start].trim_end();
    let after_block = text[start..]
        .split_once("\n\n")
        .map(|(_, remainder)| remainder.trim_start())
        .unwrap_or_default();
    match (before.is_empty(), after_block.is_empty()) {
        (true, true) => String::new(),
        (true, false) => after_block.to_string(),
        (false, true) => before.to_string(),
        (false, false) => format!("{before}\n\n{after_block}"),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        build_iteration_limit_message, build_repeated_tool_loop_message,
        sanitize_message_for_storage, strip_runtime_context_text, truncate_for_diagnostic,
    };
    use crate::engine::ContextBuilder;
    use crate::storage::ChatMessage;
    use serde_json::json;

    #[test]
    fn iteration_limit_message_includes_recent_tool_output() {
        let messages = vec![
            ChatMessage::text("assistant", "Plan the edit"),
            ChatMessage {
                role: "tool".to_string(),
                content: Some(serde_json::Value::String(
                    "Error: file not found".to_string(),
                )),
                tool_calls: None,
                tool_call_id: Some("call_1".to_string()),
                name: Some("read_file".to_string()),
                timestamp: None,
                reasoning_content: None,
                thinking_blocks: None,
                metadata: None,
            },
        ];

        let message = build_iteration_limit_message(64, &messages, Some("Inspect workspace"));
        assert!(message.contains("tool-call limit (64)"));
        assert!(message.contains("Last assistant intent: Inspect workspace"));
        assert!(message.contains("read_file: Error: file not found"));
    }

    #[test]
    fn repeated_tool_loop_message_mentions_stuck_state() {
        let messages = vec![ChatMessage {
            role: "tool".to_string(),
            content: Some(serde_json::Value::String("Permission denied".to_string())),
            tool_calls: None,
            tool_call_id: Some("call_2".to_string()),
            name: Some("exec".to_string()),
            timestamp: None,
            reasoning_content: None,
            thinking_blocks: None,
            metadata: None,
        }];

        let message = build_repeated_tool_loop_message(3, &messages, None);
        assert!(message.contains(
            "**Stopped because the same tool-call pattern repeated 3 times without reaching a final answer.**"
        ));
        assert!(message.contains("The agent appears stuck"));
        assert!(message.contains("exec: Permission denied"));
    }

    #[test]
    fn diagnostic_truncation_adds_ellipsis() {
        let text = "a".repeat(300);
        let truncated = truncate_for_diagnostic(&text, 32);
        assert_eq!(truncated.len(), 35);
        assert!(truncated.ends_with("..."));
    }

    #[test]
    fn strips_runtime_context_from_persisted_user_text() {
        let message = ChatMessage::text(
            "user",
            format!(
                "continue investigating\n\n{}\nCurrent Time: now\nChannel: cli\nChat ID: direct",
                crate::engine::ContextBuilder::RUNTIME_CONTEXT_TAG
            ),
        );
        let stored = sanitize_message_for_storage(message).expect("message should persist");
        assert_eq!(
            stored.content_as_text().as_deref(),
            Some("continue investigating")
        );
    }

    #[test]
    fn strips_runtime_context_block_from_persisted_user_media_messages() {
        let message = ChatMessage {
            role: "user".to_string(),
            content: Some(json!([
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,abc"}},
                {"type": "text", "text": "look at this diagram"},
                {
                    "type": "text",
                    "text": format!(
                        "{}\nCurrent Time: now\nChannel: cli\nChat ID: direct",
                        crate::engine::ContextBuilder::RUNTIME_CONTEXT_TAG
                    )
                }
            ])),
            tool_calls: None,
            tool_call_id: None,
            name: None,
            timestamp: None,
            reasoning_content: None,
            thinking_blocks: None,
            metadata: None,
        };
        let stored = sanitize_message_for_storage(message).expect("message should persist");
        assert_eq!(
            stored
                .content
                .as_ref()
                .and_then(serde_json::Value::as_array)
                .map(Vec::len),
            Some(2)
        );
    }

    #[test]
    fn strip_runtime_context_text_returns_empty_without_user_content() {
        let stripped = strip_runtime_context_text(ContextBuilder::RUNTIME_CONTEXT_TAG);
        assert!(stripped.is_empty());
    }

    #[test]
    fn trim_unpaired_removes_assistant_with_unanswered_tool_calls() {
        use super::trim_unpaired_tool_tail;

        let mut msgs = vec![
            ChatMessage::text("user", "hello"),
            ChatMessage {
                role: "assistant".to_string(),
                content: Some(json!("checking")),
                tool_calls: Some(vec![
                    json!({"id": "c1", "type": "function", "function": {"name": "read_file", "arguments": "{}"}}),
                ]),
                tool_call_id: None,
                name: None,
                timestamp: None,
                reasoning_content: None,
                thinking_blocks: None,
                metadata: None,
            },
        ];
        trim_unpaired_tool_tail(&mut msgs);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, "user");
    }

    #[test]
    fn trim_unpaired_removes_partial_tool_results() {
        use super::trim_unpaired_tool_tail;

        let mut msgs = vec![
            ChatMessage::text("user", "go"),
            ChatMessage {
                role: "assistant".to_string(),
                content: None,
                tool_calls: Some(vec![
                    json!({"id": "c1", "type": "function", "function": {"name": "a", "arguments": "{}"}}),
                    json!({"id": "c2", "type": "function", "function": {"name": "b", "arguments": "{}"}}),
                ]),
                tool_call_id: None,
                name: None,
                timestamp: None,
                reasoning_content: None,
                thinking_blocks: None,
                metadata: None,
            },
            ChatMessage {
                role: "tool".to_string(),
                content: Some(json!("ok")),
                tool_calls: None,
                tool_call_id: Some("c1".to_string()),
                name: Some("a".to_string()),
                timestamp: None,
                reasoning_content: None,
                thinking_blocks: None,
                metadata: None,
            },
        ];
        trim_unpaired_tool_tail(&mut msgs);
        assert_eq!(
            msgs.len(),
            1,
            "assistant + partial tool result both removed"
        );
        assert_eq!(msgs[0].role, "user");
    }

    #[test]
    fn trim_unpaired_keeps_complete_pairs() {
        use super::trim_unpaired_tool_tail;

        let mut msgs = vec![
            ChatMessage::text("user", "go"),
            ChatMessage {
                role: "assistant".to_string(),
                content: None,
                tool_calls: Some(vec![
                    json!({"id": "c1", "type": "function", "function": {"name": "a", "arguments": "{}"}}),
                ]),
                tool_call_id: None,
                name: None,
                timestamp: None,
                reasoning_content: None,
                thinking_blocks: None,
                metadata: None,
            },
            ChatMessage {
                role: "tool".to_string(),
                content: Some(json!("ok")),
                tool_calls: None,
                tool_call_id: Some("c1".to_string()),
                name: Some("a".to_string()),
                timestamp: None,
                reasoning_content: None,
                thinking_blocks: None,
                metadata: None,
            },
            ChatMessage::text("assistant", "done"),
        ];
        trim_unpaired_tool_tail(&mut msgs);
        assert_eq!(msgs.len(), 4, "complete pairs must not be trimmed");
    }

    #[test]
    fn trim_unpaired_removes_nested_incomplete_after_complete() {
        use super::trim_unpaired_tool_tail;

        let mut msgs = vec![
            ChatMessage::text("user", "go"),
            ChatMessage {
                role: "assistant".to_string(),
                content: None,
                tool_calls: Some(vec![
                    json!({"id": "c1", "type": "function", "function": {"name": "a", "arguments": "{}"}}),
                ]),
                tool_call_id: None,
                name: None,
                timestamp: None,
                reasoning_content: None,
                thinking_blocks: None,
                metadata: None,
            },
            ChatMessage {
                role: "tool".to_string(),
                content: Some(json!("ok")),
                tool_calls: None,
                tool_call_id: Some("c1".to_string()),
                name: Some("a".to_string()),
                timestamp: None,
                reasoning_content: None,
                thinking_blocks: None,
                metadata: None,
            },
            ChatMessage {
                role: "assistant".to_string(),
                content: None,
                tool_calls: Some(vec![
                    json!({"id": "c2", "type": "function", "function": {"name": "b", "arguments": "{}"}}),
                ]),
                tool_call_id: None,
                name: None,
                timestamp: None,
                reasoning_content: None,
                thinking_blocks: None,
                metadata: None,
            },
        ];
        trim_unpaired_tool_tail(&mut msgs);
        assert_eq!(msgs.len(), 3, "only the second incomplete group removed");
        assert_eq!(msgs[2].role, "tool");
    }
}
