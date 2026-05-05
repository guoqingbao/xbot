use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context, Result};
use futures::future::join_all;
use regex::Regex;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::Semaphore;

use crate::config::{ExecToolConfig, WebSearchConfig};
use crate::cron::CronService;
use crate::engine::{
    ContextBuilder, MemoryConsolidator, MemoryEntry, MemoryEntryKind, SkillsLoader, SubagentManager,
};
use crate::integrations::mcp::register_mcp_tools;
use crate::providers::{LlmResponse, ProviderModelInfo, SharedProvider, TextStreamCallback};
use crate::storage::{
    ChatMessage, InboundMessage, MessageBus, OutboundMessage, Session, SessionManager,
};
use crate::tools::{
    CronTool, EditFileTool, ExecTool, ListDirTool, MessageSendCallback, MessageTool, ReadFileTool,
    SpawnTool, ToolOutput, ToolRegistry, WebFetchTool, WebSearchTool, WriteFileTool,
};
use crate::util::build_status_content;

pub type ModelSwitchCallback = Arc<dyn Fn(String, Option<usize>) -> Result<()> + Send + Sync>;

const NANOBOT_STYLE_HELP: &str = "Available commands:\n\
  /help     - Show this help message\n\
  /status   - Show current session status\n\
  /new      - Clear current session and start fresh\n\
  /stop     - Cancel current processing\n\
  /model    - Switch model (e.g. /model gpt-4.1)\n\
  /memorize - Save important facts to long-term memory";

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
    cron_tool: Option<Arc<CronTool>>,
    start_time: Instant,
    last_usage: Mutex<(usize, usize)>,
    tool_semaphore: Arc<Semaphore>,
    cancellations: Arc<Mutex<HashSet<String>>>,
    active_turns: Arc<Mutex<BTreeMap<String, usize>>>,
    stop_notifications: Arc<Mutex<BTreeMap<String, StopNotification>>>,
    announced_sessions: Arc<Mutex<HashSet<String>>>,
    model_switch_callback: Arc<Mutex<Option<ModelSwitchCallback>>>,
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
        let workspace = workspace.as_ref().to_path_buf();
        let context = ContextBuilder::new(&workspace, max_memory_bytes)?;
        let sessions = SessionManager::new(&workspace)?;
        let memory = MemoryConsolidator::new(&workspace, context_window_tokens, max_memory_bytes)?;
        let resolved_model = model
            .clone()
            .unwrap_or_else(|| provider.default_model().to_string());
        let resolved_context_window_tokens = provider
            .list_models()
            .await
            .ok()
            .and_then(|models| find_model_context_window_tokens(&models, &resolved_model))
            .unwrap_or(context_window_tokens);
        let subagents = SubagentManager::new(
            provider.clone(),
            workspace.clone(),
            MessageBus::new(64),
            resolved_model.clone(),
            web_search.clone(),
            web_proxy.clone(),
            exec.clone(),
            restrict_to_workspace,
        );
        let mut tools = ToolRegistry::new();
        let allowed_dir = restrict_to_workspace.then(|| workspace.clone());
        tools.register(Arc::new(ReadFileTool::new(
            Some(workspace.clone()),
            allowed_dir.clone(),
            vec![],
        )));
        tools.register(Arc::new(WriteFileTool::new(
            Some(workspace.clone()),
            allowed_dir.clone(),
        )));
        tools.register(Arc::new(EditFileTool::new(
            Some(workspace.clone()),
            allowed_dir.clone(),
        )));
        tools.register(Arc::new(ListDirTool::new(
            Some(workspace.clone()),
            allowed_dir.clone(),
        )));
        if exec.enable {
            tools.register(Arc::new(ExecTool::new(
                exec.timeout,
                Some(workspace.clone()),
                restrict_to_workspace,
                exec.path_append.clone(),
            )));
        }
        tools.register(Arc::new(WebSearchTool::new(web_search, web_proxy.clone())));
        tools.register(Arc::new(WebFetchTool::new(50_000, web_proxy)));
        let message_tool = Arc::new(MessageTool::new(None));
        tools.register(message_tool.clone());
        let spawn_tool = Arc::new(SpawnTool::new(subagents.clone()));
        tools.register(spawn_tool.clone());
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
            cron_tool,
            start_time: Instant::now(),
            last_usage: Mutex::new((0, 0)),
            tool_semaphore,
            cancellations: Arc::new(Mutex::new(HashSet::new())),
            active_turns: Arc::new(Mutex::new(BTreeMap::new())),
            stop_notifications: Arc::new(Mutex::new(BTreeMap::new())),
            announced_sessions: Arc::new(Mutex::new(HashSet::new())),
            model_switch_callback: Arc::new(Mutex::new(None)),
        })
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

    pub fn set_model_switch_callback(&self, callback: Option<ModelSwitchCallback>) {
        *self
            .model_switch_callback
            .lock()
            .expect("model switch callback lock poisoned") = callback;
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
                self.memory
                    .maybe_consolidate_by_tokens(&mut session, context_window_tokens)?;
                session.clear();
                sessions.save(&session)?;
                self.reset_session_announcement(session_key);
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
            _ => {}
        }

        self.memory
            .maybe_consolidate_by_tokens(&mut session, context_window_tokens)?;
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
            Ok(models) => models,
            Err(_) => return Ok(()),
        };
        let Some(resolved_model) = resolve_runtime_model_info(&models, &session_model) else {
            return Ok(());
        };
        let resolved_context_window_tokens = resolved_model.context_window_tokens;
        let model_changed = resolved_model.id != session_model;
        let context_changed = resolved_context_window_tokens
            .is_some_and(|value| Some(value) != stored_context_window_tokens);
        if !model_changed && !context_changed {
            return Ok(());
        }

        {
            let mut sessions = self.sessions.lock().expect("session manager lock poisoned");
            let mut session = sessions.get_or_create(session_key)?;
            session.metadata.insert(
                SESSION_MODEL_KEY.to_string(),
                Value::String(resolved_model.id.clone()),
            );
            if let Some(context_window_tokens) = resolved_context_window_tokens {
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
            let _ = callback(
                resolved_model.id.clone(),
                resolved_model.context_window_tokens,
            );
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
        self.process_direct_stream(content, session_key, channel, chat_id, None)
            .await
    }

    pub async fn process_direct_stream(
        &self,
        content: &str,
        session_key: &str,
        channel: &str,
        chat_id: &str,
        text_stream: Option<TextStreamCallback>,
    ) -> Result<Option<OutboundMessage>> {
        self.process_inbound_with_stream(
            InboundMessage {
                channel: channel.to_string(),
                sender_id: "user".to_string(),
                chat_id: chat_id.to_string(),
                content: content.to_string(),
                timestamp: chrono::Utc::now(),
                media: Vec::new(),
                metadata: BTreeMap::new(),
                session_key_override: Some(session_key.to_string()),
            },
            text_stream,
        )
        .await
    }

    pub async fn process_inbound(&self, msg: InboundMessage) -> Result<Option<OutboundMessage>> {
        self.process_inbound_with_stream(msg, None).await
    }

    async fn process_inbound_with_stream(
        &self,
        msg: InboundMessage,
        text_stream: Option<TextStreamCallback>,
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
            .set_context(&msg.channel, &msg.chat_id, &session_key);
        if let Some(cron_tool) = &self.cron_tool {
            cron_tool.set_context(&msg.channel, &msg.chat_id);
        }

        let session_key = msg.session_key();
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
        )?;

        let loop_result = {
            let _guard = ActiveTurnGuard::new(self.active_turns.clone(), session_key.clone());
            self.run_agent_loop(
                &session_key,
                &active_model,
                initial_messages.clone(),
                text_stream,
                Some(target.clone()),
            )
            .await
        };
        let (final_content, all_messages, interrupted, final_reasoning_content, completed_normally) =
            match loop_result {
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
            self.save_turn(&mut session, &all_messages)?;
            self.memory
                .maybe_consolidate_by_tokens(&mut session, context_window_tokens)?;
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
        if completed_normally {
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
        let (channel, chat_id) = msg
            .chat_id
            .split_once(':')
            .map(|(channel, chat_id)| (channel.to_string(), chat_id.to_string()))
            .unwrap_or_else(|| ("cli".to_string(), msg.chat_id.clone()));
        let session_key = format!("{channel}:{chat_id}");
        let progress_target = ProgressTarget {
            channel: channel.clone(),
            chat_id: chat_id.clone(),
            session_key: session_key.clone(),
            metadata: BTreeMap::new(),
        };

        {
            let mut sessions = self.sessions.lock().expect("session manager lock poisoned");
            let mut session = sessions.get_or_create(&session_key)?;
            let context_window_tokens = self.session_context_window_tokens(&session);
            self.memory
                .maybe_consolidate_by_tokens(&mut session, context_window_tokens)?;
            sessions.put(session);
        }

        self.message_tool.set_context(&channel, &chat_id, None);
        self.message_tool.start_turn();
        self.spawn_tool
            .set_context(&channel, &chat_id, &session_key);
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
        )?;

        let loop_result = {
            let _guard = ActiveTurnGuard::new(self.active_turns.clone(), session_key.clone());
            self.run_agent_loop(
                &session_key,
                &active_model,
                initial_messages.clone(),
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
            self.save_turn(&mut session, &all_messages)?;
            self.memory
                .maybe_consolidate_by_tokens(&mut session, context_window_tokens)?;
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
        mut messages: Vec<ChatMessage>,
        text_stream: Option<TextStreamCallback>,
        progress_target: Option<ProgressTarget>,
    ) -> Result<(Option<String>, Vec<ChatMessage>, bool, Option<String>, bool)> {
        *self.last_usage.lock().expect("usage lock poisoned") = (0, 0);
        let mut final_content = None;
        let mut final_reasoning_content = None;
        let mut completed_normally = false;
        let think_re = Regex::new(r"(?s)<think>.*?</think>").expect("valid think regex");
        let mut last_tool_call_fingerprint: Option<String> = None;
        let mut repeated_tool_call_streak = 0_usize;
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
                    return Ok((None, messages, true, None, false));
                }
            }

            if self.max_iterations > 0 && iteration >= self.max_iterations {
                break;
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
                )
                .await?;
            self.record_usage(&response);

            // Check for cancellation immediately after LLM response
            {
                if self
                    .cancellations
                    .lock()
                    .expect("cancellations lock poisoned")
                    .contains(session_key)
                {
                    return Ok((None, messages, true, None, false));
                }
            }

            if response.has_tool_calls() {
                let tool_call_fingerprint = normalize_tool_call_fingerprint(&response.tool_calls);
                if last_tool_call_fingerprint.as_deref() == Some(tool_call_fingerprint.as_str()) {
                    repeated_tool_call_streak += 1;
                } else {
                    repeated_tool_call_streak = 1;
                    last_tool_call_fingerprint = Some(tool_call_fingerprint);
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
                self.context.add_assistant_message(
                    &mut messages,
                    assistant_content,
                    Some(openai_tool_calls),
                    response.reasoning_content.clone(),
                    response.thinking_blocks.clone(),
                );

                let tool_call_requests = response.tool_calls;

                for tool_call in &tool_call_requests {
                    // Check for cancellation before each tool call
                    {
                        if self
                            .cancellations
                            .lock()
                            .expect("cancellations lock poisoned")
                            .contains(session_key)
                        {
                            return Ok((None, messages, true, None, false));
                        }
                    }
                    self.send_tool_hint(progress_target.as_ref(), tool_call)
                        .await;
                }

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
                    self.context.add_tool_result(
                        &mut messages,
                        &tool_call.id,
                        &tool_call.name,
                        output.into_value(),
                    );
                }

                // Break the main loop if cancellation was detected during tool execution
                {
                    if self
                        .cancellations
                        .lock()
                        .expect("cancellations lock poisoned")
                        .contains(session_key)
                    {
                        return Ok((None, messages, true, None, false));
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
            } else {
                let content = response
                    .content
                    .clone()
                    .map(|text| think_re.replace_all(&text, "").trim().to_string())
                    .filter(|text| !text.is_empty());
                if let Some(content) = &content {
                    last_assistant_content = Some(content.clone());
                }
                self.context.add_assistant_message(
                    &mut messages,
                    content.clone(),
                    None,
                    response.reasoning_content.clone(),
                    response.thinking_blocks.clone(),
                );
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
                return Ok((None, messages, true, None, false));
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
        ))
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
        let (prompt_tokens, completion_tokens) =
            *self.last_usage.lock().expect("usage lock poisoned");
        let context_tokens = self.memory.estimate_session_prompt_tokens(session);
        let active_model = self.session_model(session);
        let context_window_tokens = self.session_context_window_tokens(session);
        build_status_content(
            env!("CARGO_PKG_VERSION"),
            &active_model,
            &self.workspace.display().to_string(),
            self.start_time.elapsed().as_secs(),
            prompt_tokens,
            completion_tokens,
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
    }

    fn save_turn(&self, session: &mut Session, messages: &[ChatMessage]) -> Result<()> {
        let skip = 1 + session.get_history(0).len();
        for message in messages.iter().skip(skip) {
            if message.role == "assistant"
                && message.content.is_none()
                && message.tool_calls.as_ref().is_none_or(Vec::is_empty)
            {
                continue;
            }
            let mut stored = message.clone();
            if stored.timestamp.is_none() {
                stored.timestamp = Some(crate::util::now_iso());
            }
            let Some(stored) = sanitize_message_for_storage(stored) else {
                continue;
            };
            if let Some(Value::String(text)) = &stored.content {
                if text.trim().is_empty() {
                    continue;
                }
            }
            session.messages.push(stored);
        }
        session.updated_at = crate::util::now_iso();
        Ok(())
    }

    fn persist_session_messages(&self, session_key: &str, messages: &[ChatMessage]) -> Result<()> {
        let mut sessions = self.sessions.lock().expect("session manager lock poisoned");
        let mut session = sessions.get_or_create(session_key)?;
        self.save_turn(&mut session, messages)?;
        let context_window_tokens = self.session_context_window_tokens(&session);
        self.memory
            .maybe_consolidate_by_tokens(&mut session, context_window_tokens)?;
        sessions.save(&session)?;
        Ok(())
    }

    #[allow(dead_code)]
    pub(crate) async fn consolidate_session_async(&self, session_key: &str) {
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
        let (last_prompt_tokens, last_completion_tokens) =
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
        })
    }

    pub fn session_summaries(&self) -> Result<Vec<crate::storage::SessionSummary>> {
        let sessions = self.sessions.lock().expect("session manager lock poisoned");
        sessions.list_session_summaries()
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

fn sanitize_message_for_storage(mut message: ChatMessage) -> Option<ChatMessage> {
    match &mut message.content {
        Some(Value::String(text)) => {
            if text.starts_with(ContextBuilder::RUNTIME_CONTEXT_TAG) {
                if message.role == "user" {
                    *text = strip_runtime_context_text(text);
                } else {
                    return None;
                }
            }
        }
        Some(Value::Array(blocks)) => {
            if message.role == "user"
                && blocks
                    .first()
                    .and_then(|block| block.get("text"))
                    .and_then(Value::as_str)
                    .is_some_and(|text| text.starts_with(ContextBuilder::RUNTIME_CONTEXT_TAG))
            {
                blocks.remove(0);
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
    if !text.starts_with(ContextBuilder::RUNTIME_CONTEXT_TAG) {
        return text.to_string();
    }
    text.split_once("\n\n")
        .map(|(_, remainder)| remainder.to_string())
        .unwrap_or_default()
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
                "{}\nCurrent Time: now\nChannel: cli\nChat ID: direct\n\ncontinue investigating",
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
                {
                    "type": "text",
                    "text": format!(
                        "{}\nCurrent Time: now\nChannel: cli\nChat ID: direct",
                        crate::engine::ContextBuilder::RUNTIME_CONTEXT_TAG
                    )
                },
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,abc"}},
                {"type": "text", "text": "look at this diagram"}
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
}
