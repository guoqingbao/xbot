use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use regex::Regex;

use crate::config::{ExecToolConfig, WebSearchConfig};
use crate::engine::SkillsLoader;
use crate::providers::SharedProvider;
use crate::storage::{ChatMessage, InboundMessage, MessageBus};
use crate::tools::{
    ApprovalCallback, EditFileTool, ExecTool, GrepFilesTool, ListDirTool, ReadFileTool, ToolOutput,
    ToolRegistry, WebFetchTool, WebSearchTool, WriteFileTool,
};
use crate::util::workspace_state_dir;

#[derive(Debug, Clone)]
pub enum SubagentNotification {
    Started {
        task_id: String,
        label: String,
        task: String,
        model: String,
    },
    Progress {
        task_id: String,
        tool_name: String,
        detail: String,
        step: u32,
    },
    Reasoning {
        task_id: String,
        content: String,
    },
    TextDelta {
        task_id: String,
        content: String,
    },
    Completed {
        task_id: String,
        label: String,
        result_preview: String,
        full_result: String,
        prompt_tokens: usize,
        completion_tokens: usize,
        cached_tokens: usize,
    },
    Failed {
        task_id: String,
        label: String,
        error: String,
    },
    Cancelled {
        task_id: String,
    },
}

pub const DEFAULT_MAX_CONCURRENT_SUBAGENTS: usize = 3;
const SUBAGENT_MAX_ITERATIONS: usize = 100;

pub type SubagentNotificationCallback = Arc<dyn Fn(SubagentNotification) + Send + Sync>;

#[derive(Debug, Clone)]
pub struct CompletedSubagentResult {
    pub task_id: String,
    pub label: String,
    pub task: String,
    pub result: String,
}

#[derive(Clone)]
pub struct SubagentManager {
    provider: SharedProvider,
    workspace: PathBuf,
    bus: Arc<Mutex<MessageBus>>,
    model: String,
    web_search_config: WebSearchConfig,
    web_proxy: Option<String>,
    exec_config: ExecToolConfig,
    restrict_to_workspace: bool,
    memory_enabled: bool,
    context_window_tokens: usize,
    running_tasks: Arc<Mutex<BTreeMap<String, tokio::task::JoinHandle<()>>>>,
    session_tasks: Arc<Mutex<BTreeMap<String, HashSet<String>>>>,
    completed_results: Arc<Mutex<BTreeMap<String, Vec<CompletedSubagentResult>>>>,
    consumed_results: Arc<Mutex<HashSet<String>>>,
    result_notify: Arc<tokio::sync::Notify>,
    notification_callback: Arc<Mutex<Option<SubagentNotificationCallback>>>,
    approval_callback: Arc<Mutex<Option<ApprovalCallback>>>,
    always_allow: Arc<Mutex<bool>>,
    max_concurrent: usize,
    write_gate: Arc<tokio::sync::Mutex<()>>,
    globally_denied: Arc<AtomicBool>,
    cancellations: Arc<Mutex<HashSet<String>>>,
}

impl SubagentManager {
    pub fn new(
        provider: SharedProvider,
        workspace: PathBuf,
        bus: MessageBus,
        model: String,
        web_search_config: WebSearchConfig,
        web_proxy: Option<String>,
        exec_config: ExecToolConfig,
        restrict_to_workspace: bool,
        memory_enabled: bool,
        context_window_tokens: usize,
        approval_callback: Arc<Mutex<Option<ApprovalCallback>>>,
        always_allow: Arc<Mutex<bool>>,
        cancellations: Arc<Mutex<HashSet<String>>>,
    ) -> Self {
        Self {
            provider,
            workspace,
            bus: Arc::new(Mutex::new(bus)),
            model,
            web_search_config,
            web_proxy,
            exec_config,
            restrict_to_workspace,
            memory_enabled,
            context_window_tokens,
            running_tasks: Arc::new(Mutex::new(BTreeMap::new())),
            session_tasks: Arc::new(Mutex::new(BTreeMap::new())),
            completed_results: Arc::new(Mutex::new(BTreeMap::new())),
            consumed_results: Arc::new(Mutex::new(HashSet::new())),
            result_notify: Arc::new(tokio::sync::Notify::new()),
            notification_callback: Arc::new(Mutex::new(None)),
            approval_callback,
            always_allow,
            max_concurrent: DEFAULT_MAX_CONCURRENT_SUBAGENTS,
            write_gate: Arc::new(tokio::sync::Mutex::new(())),
            globally_denied: Arc::new(AtomicBool::new(false)),
            cancellations,
        }
    }

    pub fn set_max_concurrent(&mut self, max: usize) {
        self.max_concurrent = max;
    }

    pub fn is_globally_denied(&self) -> bool {
        self.globally_denied.load(Ordering::SeqCst)
    }

    pub fn reset_denied(&self) {
        self.globally_denied.store(false, Ordering::SeqCst);
    }

    pub fn set_notification_callback(&self, callback: Option<SubagentNotificationCallback>) {
        *self
            .notification_callback
            .lock()
            .expect("subagent notification lock poisoned") = callback;
    }

    pub fn set_approval_callback(&self, callback: Option<ApprovalCallback>) {
        *self
            .approval_callback
            .lock()
            .expect("subagent approval lock poisoned") = callback;
    }

    fn notify(&self, event: SubagentNotification) {
        if let Some(cb) = self
            .notification_callback
            .lock()
            .expect("subagent notification lock poisoned")
            .clone()
        {
            cb(event);
        }
    }

    pub async fn spawn(
        &self,
        task: String,
        label: Option<String>,
        origin_channel: String,
        origin_chat_id: String,
        session_key: Option<String>,
        origin_metadata: BTreeMap<String, serde_json::Value>,
    ) -> String {
        let current_count = self.get_running_count();
        if current_count >= self.max_concurrent {
            return format!(
                "Cannot spawn subagent: concurrency limit reached ({}/{}). \
                 Wait for existing subagents to complete before spawning more.",
                current_count, self.max_concurrent
            );
        }
        let task_id = uuid::Uuid::new_v4()
            .simple()
            .to_string()
            .chars()
            .take(8)
            .collect::<String>();
        let display_label = label.unwrap_or_else(|| {
            let trimmed = task.chars().take(30).collect::<String>();
            if task.chars().count() > 30 {
                format!("{trimmed}...")
            } else {
                trimmed
            }
        });
        let manager = self.clone();
        let task_id_for_spawn = task_id.clone();
        let display_label_for_spawn = display_label.clone();
        let session_key_for_cleanup = session_key.clone();
        let session_key_for_result = session_key.clone();
        let task_for_spawn = task.clone();
        let handle = tokio::spawn(async move {
            let _ = manager
                .run_subagent(
                    task_id_for_spawn.clone(),
                    task_for_spawn,
                    display_label_for_spawn,
                    origin_channel,
                    origin_chat_id,
                    session_key_for_result,
                    origin_metadata,
                )
                .await;
            manager.cleanup_task(&task_id_for_spawn, session_key_for_cleanup.as_deref());
        });
        self.running_tasks
            .lock()
            .expect("subagent running lock poisoned")
            .insert(task_id.clone(), handle);
        if let Some(session_key) = session_key {
            self.session_tasks
                .lock()
                .expect("subagent session lock poisoned")
                .entry(session_key)
                .or_default()
                .insert(task_id.clone());
        }
        self.notify(SubagentNotification::Started {
            task_id: task_id.clone(),
            label: display_label.clone(),
            task,
            model: self.model.clone(),
        });
        format!(
            "Subagent [{display_label}] started (id: {task_id}). Call `wait_subagents` to collect my results."
        )
    }

    pub async fn cancel_by_session(&self, session_key: &str) -> usize {
        self.clear_session_state(session_key, false)
    }

    pub fn reset_session(&self, session_key: &str) -> usize {
        self.globally_denied.store(false, Ordering::SeqCst);
        self.clear_session_state(session_key, true)
    }

    fn clear_session_state(&self, session_key: &str, clear_results: bool) -> usize {
        let task_ids = self
            .session_tasks
            .lock()
            .expect("subagent session lock poisoned")
            .remove(session_key)
            .unwrap_or_default()
            .into_iter()
            .collect::<Vec<_>>();
        let mut cancelled = 0;
        for task_id in &task_ids {
            if let Some(handle) = self
                .running_tasks
                .lock()
                .expect("subagent running lock poisoned")
                .remove(task_id)
            {
                handle.abort();
                self.notify(SubagentNotification::Cancelled {
                    task_id: task_id.clone(),
                });
                cancelled += 1;
            }
        }
        if clear_results {
            let cleared_results = self
                .completed_results
                .lock()
                .expect("subagent results lock poisoned")
                .remove(session_key)
                .unwrap_or_default();
            if !cleared_results.is_empty() {
                let mut consumed = self
                    .consumed_results
                    .lock()
                    .expect("subagent consumed results lock poisoned");
                for result in cleared_results {
                    consumed.insert(result.task_id);
                }
            }
        }
        self.result_notify.notify_waiters();
        cancelled
    }

    pub fn get_running_count(&self) -> usize {
        self.running_tasks
            .lock()
            .expect("subagent running lock poisoned")
            .len()
    }

    pub fn set_bus(&self, bus: MessageBus) {
        *self.bus.lock().expect("subagent bus lock poisoned") = bus;
    }

    pub async fn wait_for_session_results(
        &self,
        session_key: &str,
        timeout: Duration,
    ) -> (Vec<CompletedSubagentResult>, usize, bool) {
        let deadline = Instant::now() + timeout;
        loop {
            let running = self.running_for_session(session_key);
            if running == 0 {
                let results = self.take_completed_results(session_key);
                return (results, 0, false);
            }

            let now = Instant::now();
            if now >= deadline {
                let results = self.take_completed_results(session_key);
                return (results, running, true);
            }

            let remaining = deadline.saturating_duration_since(now);
            let _ = tokio::time::timeout(remaining, self.result_notify.notified()).await;
        }
    }

    fn running_for_session(&self, session_key: &str) -> usize {
        self.session_tasks
            .lock()
            .expect("subagent session lock poisoned")
            .get(session_key)
            .map(HashSet::len)
            .unwrap_or(0)
    }

    fn take_completed_results(&self, session_key: &str) -> Vec<CompletedSubagentResult> {
        let results = self
            .completed_results
            .lock()
            .expect("subagent results lock poisoned")
            .remove(session_key)
            .unwrap_or_default();
        if !results.is_empty() {
            let mut consumed = self
                .consumed_results
                .lock()
                .expect("subagent consumed results lock poisoned");
            for result in &results {
                consumed.insert(result.task_id.clone());
            }
        }
        results
    }

    pub fn take_consumed_result(&self, task_id: &str) -> bool {
        self.consumed_results
            .lock()
            .expect("subagent consumed results lock poisoned")
            .remove(task_id)
    }

    fn record_completed_result(
        &self,
        session_key: Option<&str>,
        task_id: String,
        label: String,
        task: String,
        result: String,
    ) {
        let Some(session_key) = session_key else {
            return;
        };
        self.completed_results
            .lock()
            .expect("subagent results lock poisoned")
            .entry(session_key.to_string())
            .or_default()
            .push(CompletedSubagentResult {
                task_id,
                label,
                task,
                result,
            });
        self.result_notify.notify_waiters();
    }

    async fn run_subagent(
        &self,
        task_id: String,
        task: String,
        label: String,
        origin_channel: String,
        origin_chat_id: String,
        origin_session_key: Option<String>,
        origin_metadata: BTreeMap<String, serde_json::Value>,
    ) -> Result<()> {
        let tools = self.build_tools();
        let think_re = Regex::new(r"(?s)<think>.*?</think>").expect("valid think regex");
        let mut messages = vec![
            ChatMessage::text("system", self.build_subagent_prompt()),
            ChatMessage::text("user", task.clone()),
        ];

        let mut final_result = None;
        let mut step: u32 = 0;
        let mut total_prompt_tokens: usize = 0;
        let mut total_completion_tokens: usize = 0;
        let mut total_cached_tokens: usize = 0;
        let mut compression_pending = false;
        let ctx_window = self.context_window_tokens;

        for _ in 0..SUBAGENT_MAX_ITERATIONS {
            if self.globally_denied.load(Ordering::SeqCst) {
                final_result =
                    Some("Subagent stopped: file modifications denied by user.".to_string());
                break;
            }

            if compression_pending && messages.len() > 3 {
                messages = compress_subagent_context(messages);
                compression_pending = false;
            }

            let response = self
                .provider
                .chat_with_retry(
                    &messages,
                    Some(&tools.definitions()),
                    Some(&self.model),
                    None,
                    None,
                )
                .await
                .map_err(|e| {
                    self.notify(SubagentNotification::Failed {
                        task_id: task_id.clone(),
                        label: label.clone(),
                        error: format!("{e:#}"),
                    });
                    e
                })?;

            total_prompt_tokens += response.usage.prompt_tokens;
            total_completion_tokens += response.usage.completion_tokens;
            total_cached_tokens += response.usage.cached_prompt_tokens;

            let needs_compression = ctx_window > 0
                && response.usage.prompt_tokens > 0
                && response.usage.prompt_tokens.saturating_mul(100)
                    >= ctx_window.saturating_mul(90);

            if let Some(ref rc) = response.reasoning_content {
                if !rc.trim().is_empty() && !response.has_tool_calls() {
                    self.notify(SubagentNotification::Reasoning {
                        task_id: task_id.clone(),
                        content: rc.clone(),
                    });
                }
            }
            if let Some(ref text) = response.content {
                if !text.trim().is_empty() && !response.has_tool_calls() {
                    self.notify(SubagentNotification::TextDelta {
                        task_id: task_id.clone(),
                        content: text.clone(),
                    });
                }
            }
            if response.has_tool_calls() {
                let tool_calls = response
                    .tool_calls
                    .iter()
                    .map(|call| call.to_openai_tool_call())
                    .collect::<Vec<_>>();
                messages.push(ChatMessage {
                    role: "assistant".to_string(),
                    content: response.content.clone().map(serde_json::Value::String),
                    tool_calls: Some(tool_calls),
                    tool_call_id: None,
                    name: None,
                    timestamp: None,
                    reasoning_content: response.reasoning_content.clone(),
                    thinking_blocks: response.thinking_blocks.clone(),
                    metadata: None,
                });
                let mut denied_path: Option<String> = None;
                for tool_call in response.tool_calls {
                    if self.globally_denied.load(Ordering::SeqCst) {
                        denied_path = Some("(globally denied)".to_string());
                        break;
                    }
                    step += 1;
                    self.notify(SubagentNotification::Progress {
                        task_id: task_id.clone(),
                        tool_name: tool_call.name.clone(),
                        detail: summarize_tool_args_for_notify(&tool_call.arguments),
                        step,
                    });
                    let output = match self
                        .check_approval(&tool_call, &task_id, &label, origin_session_key.as_deref())
                        .await
                    {
                        Some(denial_output) => {
                            let path = tool_call
                                .arguments
                                .get("path")
                                .and_then(|v| v.as_str())
                                .unwrap_or("unknown")
                                .to_string();
                            denied_path = Some(path);
                            denial_output
                        }
                        None => tools.execute(&tool_call.name, tool_call.arguments).await,
                    };
                    messages.push(ChatMessage {
                        role: "tool".to_string(),
                        content: Some(output.into_value()),
                        tool_calls: None,
                        tool_call_id: Some(tool_call.id),
                        name: Some(tool_call.name),
                        timestamp: None,
                        reasoning_content: None,
                        thinking_blocks: None,
                        metadata: None,
                    });
                    if denied_path.is_some() {
                        break;
                    }
                }
                if let Some(path) = denied_path {
                    final_result = Some(format!(
                        "Subagent stopped: user denied file modification for '{path}'."
                    ));
                    break;
                }
                if needs_compression {
                    compression_pending = true;
                }
            } else {
                final_result = response
                    .content
                    .map(|text| think_re.replace_all(&text, "").trim().to_string())
                    .filter(|text| !text.is_empty());
                break;
            }
        }

        if final_result.is_none() {
            messages.push(ChatMessage::text(
                "user",
                format!(
                    "You have reached the subagent tool-iteration budget ({SUBAGENT_MAX_ITERATIONS}). \
                     Stop using tools now and write the final text result for the delegated task. \
                     Synthesize findings from the work already done. If you did not find bugs or \
                     could not complete part of the review, say that explicitly. Do not paste raw \
                     truncated tool output as the final result."
                ),
            ));
            if let Ok(response) = self
                .provider
                .chat_with_retry(&messages, None, Some(&self.model), None, None)
                .await
            {
                total_prompt_tokens += response.usage.prompt_tokens;
                total_completion_tokens += response.usage.completion_tokens;
                total_cached_tokens += response.usage.cached_prompt_tokens;
                final_result = response
                    .content
                    .map(|text| think_re.replace_all(&text, "").trim().to_string())
                    .filter(|text| !text.is_empty());
            }
        }

        let result = final_result.unwrap_or_else(|| {
            extract_fallback_result(&messages).unwrap_or_else(|| {
                "Task completed but no final response was generated.".to_string()
            })
        });

        let preview: String = result.chars().take(200).collect();
        self.notify(SubagentNotification::Completed {
            task_id: task_id.clone(),
            label: label.clone(),
            result_preview: preview,
            full_result: result.clone(),
            prompt_tokens: total_prompt_tokens,
            completion_tokens: total_completion_tokens,
            cached_tokens: total_cached_tokens,
        });
        self.record_completed_result(
            origin_session_key.as_deref(),
            task_id.clone(),
            label.clone(),
            task.clone(),
            result.clone(),
        );

        let mut metadata = origin_metadata;
        metadata.insert(
            "task_id".to_string(),
            serde_json::Value::String(task_id.clone()),
        );

        let bus = self.bus.lock().expect("subagent bus lock poisoned").clone();
        bus
            .publish_inbound(InboundMessage {
                channel: "system".to_string(),
                sender_id: "subagent".to_string(),
                chat_id: format!("{origin_channel}:{origin_chat_id}"),
                content: format!(
                    "[Subagent '{label}' completed]\n\nTask: {task}\n\nResult:\n{result}\n\nSummarize this naturally for the user. Keep it brief and do not mention technical implementation details."
                ),
                timestamp: chrono::Utc::now(),
                media: Vec::new(),
                metadata,
                session_key_override: origin_session_key,
            })
            .await?;
        Ok(())
    }

    fn build_tools(&self) -> ToolRegistry {
        let mut tools = ToolRegistry::new();
        let allowed_dir = self.restrict_to_workspace.then(|| self.workspace.clone());
        let blocked_dirs = {
            let mut dirs = Vec::new();
            
            // Block memory directory when memory is not enabled
            if !self.memory_enabled {
                dirs.push(workspace_state_dir(&self.workspace).join("memory"));
            }
            
            // Block sessions directory to prevent xbot from reading its own sessions
            dirs.push(workspace_state_dir(&self.workspace).join("sessions"));
            
            // Block tui_input_history.json to prevent xbot from reading its own input history
            dirs.push(workspace_state_dir(&self.workspace).join("tui_input_history.json"));
            
            dirs
        };
        tools.register(Arc::new(
            ReadFileTool::new(Some(self.workspace.clone()), allowed_dir.clone(), vec![])
                .with_blocked_dirs(blocked_dirs.clone()),
        ));
        tools.register(Arc::new(
            WriteFileTool::new(Some(self.workspace.clone()), allowed_dir.clone())
                .with_blocked_dirs(blocked_dirs.clone()),
        ));
        tools.register(Arc::new(
            EditFileTool::new(Some(self.workspace.clone()), allowed_dir.clone())
                .with_blocked_dirs(blocked_dirs.clone()),
        ));
        tools.register(Arc::new(
            ListDirTool::new(Some(self.workspace.clone()), allowed_dir.clone())
                .with_blocked_dirs(blocked_dirs.clone()),
        ));
        tools.register(Arc::new(GrepFilesTool::new(
            Some(self.workspace.clone()),
            allowed_dir.clone(),
        )));
        if self.exec_config.enable {
            tools.register(Arc::new(
                ExecTool::new(
                    self.exec_config.timeout,
                    Some(self.workspace.clone()),
                    self.restrict_to_workspace,
                    self.exec_config.path_append.clone(),
                )
                .with_blocked_dirs(blocked_dirs.clone()),
            ));
        }
        tools.register(Arc::new(WebSearchTool::new(
            self.web_search_config.clone(),
            self.web_proxy.clone(),
        )));
        tools.register(Arc::new(WebFetchTool::new(50_000, self.web_proxy.clone())));
        tools
    }

    fn is_file_modifying_tool(name: &str) -> bool {
        matches!(name, "write_file" | "edit_file")
    }

    async fn check_approval(
        &self,
        tool_call: &crate::providers::ToolCallRequest,
        task_id: &str,
        label: &str,
        session_key: Option<&str>,
    ) -> Option<ToolOutput> {
        use crate::tools::{ApprovalDecision, ApprovalRequest};

        if !Self::is_file_modifying_tool(&tool_call.name) {
            return None;
        }

        if self.globally_denied.load(Ordering::SeqCst) {
            return Some(ToolOutput::Text(
                "Error: File modifications denied by user.".to_string(),
            ));
        }

        if *self
            .always_allow
            .lock()
            .expect("subagent always_allow lock poisoned")
        {
            return None;
        }

        let callback = self
            .approval_callback
            .lock()
            .expect("subagent approval lock poisoned")
            .clone()?;

        let path = tool_call
            .arguments
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();

        let diff_lines = match tool_call.name.as_str() {
            "edit_file" => {
                let old = tool_call
                    .arguments
                    .get("old_text")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                let new = tool_call
                    .arguments
                    .get("new_text")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                crate::diff::compute_diff(old, new).lines
            }
            "write_file" => {
                let content = tool_call
                    .arguments
                    .get("content")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                crate::diff::compute_write_diff(content).lines
            }
            _ => Vec::new(),
        };

        let request = ApprovalRequest {
            tool_name: tool_call.name.clone(),
            path: path.clone(),
            diff_lines,
            source: Some(format!("subagent '{label}' ({task_id})")),
        };

        let _gate = self.write_gate.lock().await;

        if self.globally_denied.load(Ordering::SeqCst) {
            return Some(ToolOutput::Text(
                "Error: File modifications denied by user.".to_string(),
            ));
        }

        if *self
            .always_allow
            .lock()
            .expect("subagent always_allow lock poisoned")
        {
            return None;
        }

        match callback(request).await {
            ApprovalDecision::AllowOnce => None,
            ApprovalDecision::AlwaysAllow => {
                *self
                    .always_allow
                    .lock()
                    .expect("subagent always_allow lock poisoned") = true;
                None
            }
            ApprovalDecision::Deny => {
                self.globally_denied.store(true, Ordering::SeqCst);
                self.cancel_all_running();
                if let Some(sk) = session_key {
                    self.cancellations
                        .lock()
                        .expect("cancellations lock poisoned")
                        .insert(sk.to_string());
                }
                Some(ToolOutput::Text(format!(
                    "Error: User denied file modification for '{path}'. All tasks stopped.",
                )))
            }
        }
    }

    fn cancel_all_running(&self) {
        let mut running = self
            .running_tasks
            .lock()
            .expect("subagent running lock poisoned");
        let task_ids: Vec<String> = running.keys().cloned().collect();
        for task_id in task_ids {
            if let Some(handle) = running.remove(&task_id) {
                handle.abort();
                self.notify(SubagentNotification::Cancelled {
                    task_id: task_id.clone(),
                });
            }
        }
        drop(running);
        self.session_tasks
            .lock()
            .expect("subagent session lock poisoned")
            .clear();
        self.result_notify.notify_waiters();
    }

    fn build_subagent_prompt(&self) -> String {
        let skills_summary = SkillsLoader::new(&self.workspace, None).build_skills_summary();
        let mut prompt = format!(
            r#"# Subagent

You are a focused background subagent working on a delegated subtask within a larger workflow.

## Environment
- Workspace: {workspace}
- Platform: {platform}

## Core Principles

1. **Efficiency over thoroughness**: Use `grep_files` to find relevant code instead of reading \
   entire files. Only `read_file` specific sections you need after locating them via search.

2. **Parallel tool calls**: Batch independent operations. If you need to search for 3 patterns, \
   issue all 3 `grep_files` in one turn. If you need to read 3 files, issue all 3 `read_file` \
   calls together.

3. **Targeted investigation**: Start with `grep_files` and `list_dir` to understand structure, \
   then drill into specific files. Never dump entire directories or large files into context.

4. **Bounded output**: Use `read_file` with offset/limit to read only the relevant section. \
   Use `grep_files` with specific patterns rather than broad wildcards.

5. **Anti-loop discipline**: NEVER call the same tool more than 5 times in succession without \
   producing a text synthesis. If you've searched 5 times, STOP and summarize your findings. \
   Do not keep searching with slight pattern variations — synthesize what you have.

## Tool Usage

- **`grep_files`**: Primary exploration tool. Use regex patterns to find definitions, usages, \
  imports, error patterns. Always prefer over reading entire files.
- **`read_file`**: Use with offset/limit after grep identifies the location. For files under \
  ~100 lines, reading whole is acceptable.
- **`list_dir`**: Survey directory structure before diving in.
- **`edit_file` / `write_file`**: For implementation tasks only.
- **`exec`**: For build commands, tests, git operations. Never use for `grep`, `cat`, or `ls` \
  — use the structured tools instead.

## Response Requirements

- You MUST end with a text response (not a tool call).
- Your final message is reported back to the parent agent as your result.
- Match the task type in your response:
  - **Research/exploration**: report findings with file paths and line numbers
  - **Implementation**: report changed files, what was done, verification results
  - **Analysis**: report conclusions with supporting evidence
  - **Bug review**: explicitly state whether bugs were found, with locations
- If no issues/findings apply, say so explicitly.
- Keep the response structured and concise — the parent needs actionable information, not a \
  narrative.
"#,
            workspace = self.workspace.display(),
            platform = std::env::consts::OS,
        );

        if !skills_summary.is_empty() {
            prompt.push_str(&format!("\n## Skills\n{skills_summary}\n"));
        }
        prompt
    }

    fn cleanup_task(&self, task_id: &str, session_key: Option<&str>) {
        self.running_tasks
            .lock()
            .expect("subagent running lock poisoned")
            .remove(task_id);
        if let Some(session_key) = session_key {
            let mut session_tasks = self
                .session_tasks
                .lock()
                .expect("subagent session lock poisoned");
            if let Some(ids) = session_tasks.get_mut(session_key) {
                ids.remove(task_id);
                if ids.is_empty() {
                    session_tasks.remove(session_key);
                }
            }
        }
        self.result_notify.notify_waiters();
    }
}

fn extract_fallback_result(messages: &[ChatMessage]) -> Option<String> {
    for msg in messages.iter().rev() {
        if msg.role == "assistant" {
            if let Some(text) = msg.content_as_text() {
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    return Some(trimmed.to_string());
                }
            }
        }
    }
    let tool_results: Vec<String> = messages
        .iter()
        .rev()
        .filter(|m| m.role == "tool")
        .filter_map(|m| {
            let text = m.content_as_text()?;
            let name = m.name.as_deref().unwrap_or("tool");
            let summary: String = text.chars().take(200).collect();
            Some(format!("{name}: {summary}"))
        })
        .take(3)
        .collect();
    if tool_results.is_empty() {
        return None;
    }
    Some(format!(
        "Subagent completed its work. Recent tool outputs:\n{}",
        tool_results
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n")
    ))
}

fn compress_subagent_context(messages: Vec<ChatMessage>) -> Vec<ChatMessage> {
    let system = messages.first().cloned();
    let user_task = messages.get(1).cloned();
    let mut summary_parts: Vec<String> = Vec::new();
    for msg in messages.iter().skip(2) {
        match msg.role.as_str() {
            "assistant" => {
                if let Some(text) = msg.content_as_text() {
                    let trimmed = text.trim();
                    if !trimmed.is_empty() {
                        let preview: String = trimmed.chars().take(300).collect();
                        summary_parts.push(format!("Assistant: {preview}"));
                    }
                }
                if let Some(ref tool_calls) = msg.tool_calls {
                    let names: Vec<_> = tool_calls
                        .iter()
                        .filter_map(|tc| {
                            tc.get("function")
                                .and_then(|f| f.get("name"))
                                .and_then(|n| n.as_str())
                        })
                        .collect();
                    if !names.is_empty() {
                        summary_parts.push(format!("Called tools: {}", names.join(", ")));
                    }
                }
            }
            "tool" => {
                let name = msg.name.as_deref().unwrap_or("tool");
                if let Some(text) = msg.content_as_text() {
                    let preview: String = text.chars().take(200).collect();
                    summary_parts.push(format!("{name} result: {preview}"));
                }
            }
            _ => {}
        }
    }
    let summary_text = if summary_parts.is_empty() {
        "Previous conversation compressed. Continue from where you left off.".to_string()
    } else {
        format!(
            "[Context compressed — summary of prior work]\n{}",
            summary_parts.join("\n")
        )
    };
    let mut result = Vec::new();
    if let Some(sys) = system {
        result.push(sys);
    }
    if let Some(user) = user_task {
        result.push(user);
    }
    result.push(ChatMessage::text("user", summary_text));
    result
}

fn summarize_tool_args_for_notify(args: &serde_json::Value) -> String {
    let preferred = [
        "path",
        "target_file",
        "file",
        "command",
        "cmd",
        "url",
        "query",
    ];
    if let Some(map) = args.as_object() {
        for key in preferred {
            if let Some(val) = map.get(key).and_then(|v| v.as_str()) {
                let truncated: String = val.chars().take(60).collect();
                return format!("{key}={truncated}");
            }
        }
        if let Some((key, val)) = map.iter().next() {
            let s = val
                .as_str()
                .map(|s| s.chars().take(40).collect::<String>())
                .unwrap_or_else(|| format!("{val}").chars().take(40).collect());
            return format!("{key}={s}");
        }
    }
    String::new()
}
