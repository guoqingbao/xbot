use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind};
use serde_json::Value;

use xbot::util::{ensure_dir, tool_emoji, workspace_state_dir};

const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
const COMPOSER_HISTORY_LIMIT: usize = 10;
const COMPOSER_HISTORY_FILE: &str = "tui_input_history.json";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentState {
    Ready,
    Working,
    WaitingSubagents,
    Summarizing,
}

#[derive(Clone)]
pub struct TurnSummary {
    pub prompt_tokens: usize,
    pub completion_tokens: usize,
    pub cached_tokens: usize,
    pub elapsed: Duration,
}

pub enum EngineEvent {
    StreamDelta(String),
    ReasoningDelta(String),
    ToolHint {
        tool_name: Option<String>,
        tool_args: Option<Value>,
    },
    ToolResult {
        tool_name: String,
        success: bool,
        summary: String,
    },
    TurnComplete {
        content: String,
        reasoning: Option<String>,
        summary: TurnSummary,
    },
    TurnEmpty {
        note: String,
        summary: TurnSummary,
    },
    TurnError(String),
    ContextUpdate(String),
    Summarizing,
    SummarizingDone,
    SubagentStarted {
        task_id: String,
        label: String,
        task: String,
        model: String,
    },
    SubagentProgress {
        task_id: String,
        tool_name: String,
        detail: String,
        step: u32,
    },
    SubagentReasoning {
        task_id: String,
        content: String,
    },
    SubagentTextDelta {
        task_id: String,
        content: String,
    },
    SubagentCompleted {
        task_id: String,
        label: String,
        result_preview: String,
        full_result: String,
    },
    SubagentFailed {
        task_id: String,
        #[allow(dead_code)]
        label: String,
        error: String,
    },
    SubagentCancelled {
        task_id: String,
    },
    CollapseThinking,
    ApprovalRequest {
        tool_name: String,
        path: String,
        diff_lines: Vec<xbot::diff::DiffLine>,
        source: Option<String>,
        responder: tokio::sync::oneshot::Sender<xbot::tools::ApprovalDecision>,
    },
}

#[derive(Clone)]
pub struct EditDiff {
    pub path: String,
    pub lines: Vec<xbot::diff::DiffLine>,
}

#[derive(Clone)]
pub enum HistoryEntry {
    User(String),
    Assistant {
        content: String,
        reasoning: Option<String>,
    },
    Thinking(String),
    ToolCall {
        name: String,
        emoji: String,
        detail: String,
        diff: Option<EditDiff>,
        result_summary: Option<(bool, String)>,
    },
    Error(String),
    System(String),
    Separator {
        summary: TurnSummary,
    },
    SubagentCard {
        task_id: String,
        label: String,
        model: String,
        status: SubagentStatus,
        actions: Vec<String>,
        result_preview: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SubagentStatus {
    Running,
    Completed,
    Failed,
    Cancelled,
}

pub struct ToolActivity {
    pub name: String,
    pub emoji: String,
    pub detail: String,
    pub diff: Option<EditDiff>,
    pub result_summary: Option<(bool, String)>,
    pub started_at: std::time::Instant,
    pub timeout_secs: Option<u64>,
}

pub struct QueuedPrompt {
    pub prompt: String,
    pub show_in_history: bool,
}

impl QueuedPrompt {
    pub fn user(prompt: String) -> Self {
        Self {
            prompt,
            show_in_history: true,
        }
    }

    pub fn internal(prompt: String) -> Self {
        Self {
            prompt,
            show_in_history: false,
        }
    }
}

pub enum StreamSegment {
    Text(String),
    Thinking(String),
    Tool(ToolActivity),
    Subagent { task_id: String, label: String },
}

pub struct ActiveStreaming {
    pub segments: Vec<StreamSegment>,
}

impl Default for ActiveStreaming {
    fn default() -> Self {
        Self {
            segments: Vec::new(),
        }
    }
}

impl ActiveStreaming {
    pub fn push_text(&mut self, text: &str) {
        if let Some(StreamSegment::Text(s)) = self.segments.last_mut() {
            s.push_str(text);
        } else {
            self.segments.push(StreamSegment::Text(text.to_string()));
        }
    }

    pub fn push_thinking(&mut self, text: &str) {
        if let Some(StreamSegment::Thinking(s)) = self.segments.last_mut() {
            s.push_str(text);
        } else {
            self.segments
                .push(StreamSegment::Thinking(text.to_string()));
        }
    }

    pub fn push_tool(&mut self, tool: ToolActivity) {
        self.segments.push(StreamSegment::Tool(tool));
    }

    pub fn push_subagent(&mut self, task_id: String, label: String) {
        self.segments
            .push(StreamSegment::Subagent { task_id, label });
    }

    pub fn has_content(&self) -> bool {
        self.segments.iter().any(|s| match s {
            StreamSegment::Text(t) | StreamSegment::Thinking(t) => !t.is_empty(),
            StreamSegment::Tool(_) | StreamSegment::Subagent { .. } => true,
        })
    }

    pub fn has_text_content(&self) -> bool {
        self.segments.iter().any(|s| match s {
            StreamSegment::Text(t) => !t.trim().is_empty(),
            _ => false,
        })
    }

    pub fn has_thinking_content(&self) -> bool {
        self.segments.iter().any(|s| match s {
            StreamSegment::Thinking(t) => !t.trim().is_empty(),
            _ => false,
        })
    }
}

#[derive(Clone)]
#[allow(dead_code)]
pub struct SubagentInfo {
    pub task_id: String,
    pub label: String,
    pub task: String,
    pub model: String,
    pub status: SubagentStatus,
    pub actions: Vec<String>,
    pub all_actions: Vec<String>,
    pub result_preview: Option<String>,
    pub full_result: Option<String>,
    pub reasoning_chunks: Vec<String>,
    pub text_chunks: Vec<String>,
    pub started_at: Instant,
    pub finished_at: Option<Instant>,
}

pub struct LineBuffer {
    pending: String,
}

impl LineBuffer {
    pub fn new() -> Self {
        Self {
            pending: String::new(),
        }
    }

    #[allow(dead_code)]
    pub fn push(&mut self, text: &str) {
        self.pending.push_str(text);
    }

    #[allow(dead_code)]
    pub fn take_committable(&mut self) -> String {
        let Some(last_nl) = self.pending.rfind('\n') else {
            return String::new();
        };
        self.pending.drain(..=last_nl).collect()
    }

    pub fn flush(&mut self) -> String {
        std::mem::take(&mut self.pending)
    }

    #[allow(dead_code)]
    pub fn clear(&mut self) {
        self.pending.clear();
    }

    pub fn pending_preview(&self) -> &str {
        &self.pending
    }
}

pub struct ComposerState {
    pub input: String,
    pub cursor: usize,
    pub history: Vec<String>,
    pub history_index: Option<usize>,
    pub draft: Option<String>,
}

impl ComposerState {
    #[cfg(test)]
    fn new() -> Self {
        Self::with_history(Vec::new())
    }

    fn with_history(history: Vec<String>) -> Self {
        Self {
            input: String::new(),
            cursor: 0,
            history: trim_composer_history(history),
            history_index: None,
            draft: None,
        }
    }

    pub fn insert_char(&mut self, ch: char) {
        let byte_pos = self.char_to_byte(self.cursor);
        self.input.insert(byte_pos, ch);
        self.cursor += 1;
    }

    pub fn insert_newline(&mut self) {
        self.insert_char('\n');
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            let byte_pos = self.char_to_byte(self.cursor);
            self.input.remove(byte_pos);
        }
    }

    pub fn delete(&mut self) {
        let total = self.input.chars().count();
        if self.cursor < total {
            let byte_pos = self.char_to_byte(self.cursor);
            self.input.remove(byte_pos);
        }
    }

    pub fn move_left(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
        }
    }

    pub fn move_right(&mut self) {
        let total = self.input.chars().count();
        if self.cursor < total {
            self.cursor += 1;
        }
    }

    pub fn move_home(&mut self) {
        let before = &self.input[..self.char_to_byte(self.cursor)];
        if let Some(pos) = before.rfind('\n') {
            self.cursor = self.input[..=pos].chars().count();
        } else {
            self.cursor = 0;
        }
    }

    pub fn move_end(&mut self) {
        let byte_pos = self.char_to_byte(self.cursor);
        let after = &self.input[byte_pos..];
        if let Some(pos) = after.find('\n') {
            self.cursor += after[..pos].chars().count();
        } else {
            self.cursor = self.input.chars().count();
        }
    }

    pub fn delete_word_back(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let byte_pos = self.char_to_byte(self.cursor);
        let before = &self.input[..byte_pos];
        let trimmed = before.trim_end();
        let word_start = trimmed
            .rfind(|c: char| c.is_whitespace() || c == '/' || c == '.')
            .map(|i| i + 1)
            .unwrap_or(0);
        let new_cursor = self.input[..word_start].chars().count();
        self.input.replace_range(word_start..byte_pos, "");
        self.cursor = new_cursor;
    }

    pub fn clear_line(&mut self) {
        self.input.clear();
        self.cursor = 0;
    }

    pub fn take_input(&mut self) -> String {
        let text = std::mem::take(&mut self.input);
        self.cursor = 0;
        self.history_index = None;
        self.draft = None;
        if !text.trim().is_empty() {
            self.history.push(text.clone());
            trim_composer_history_in_place(&mut self.history);
        }
        text
    }

    pub fn history_up(&mut self) {
        if self.history.is_empty() {
            return;
        }
        match self.history_index {
            None => {
                self.draft = Some(self.input.clone());
                self.history_index = Some(self.history.len() - 1);
            }
            Some(0) => return,
            Some(ref mut idx) => *idx -= 1,
        }
        if let Some(idx) = self.history_index {
            self.input = self.history[idx].clone();
            self.cursor = self.input.chars().count();
        }
    }

    pub fn history_down(&mut self) {
        let Some(idx) = self.history_index else {
            return;
        };
        if idx + 1 >= self.history.len() {
            self.history_index = None;
            self.input = self.draft.take().unwrap_or_default();
        } else {
            self.history_index = Some(idx + 1);
            self.input = self.history[idx + 1].clone();
        }
        self.cursor = self.input.chars().count();
    }

    pub fn insert_paste(&mut self, text: &str) {
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        for ch in normalized.chars() {
            self.insert_char(ch);
        }
    }

    fn char_to_byte(&self, char_idx: usize) -> usize {
        self.input
            .char_indices()
            .nth(char_idx)
            .map(|(i, _)| i)
            .unwrap_or(self.input.len())
    }
}

pub struct App {
    pub history: Vec<HistoryEntry>,
    pub active: Option<ActiveStreaming>,
    pub composer: ComposerState,
    pub line_buffer: LineBuffer,

    pub scroll_offset: usize,
    pub auto_scroll: bool,
    pub total_lines: usize,

    pub show_help: bool,
    pub show_sidebar: bool,
    pub show_subagent_overlay: bool,
    pub subagent_overlay_index: usize,
    pub subagent_overlay_scroll: usize,
    pub should_quit: bool,
    pub agent_state: AgentState,
    pub needs_redraw: bool,
    pub cancel_requested: bool,
    pub pending: VecDeque<QueuedPrompt>,
    pub exit_after_turn: bool,

    pub model: String,
    pub configured_subagent_model: Option<String>,
    pub provider: String,
    #[allow(dead_code)]
    pub workspace: PathBuf,
    pub session_msg_count: usize,
    pub context_status: String,
    pub last_summary: Option<TurnSummary>,
    pub animation_frame: u16,

    pub subagents: BTreeMap<String, SubagentInfo>,
    pub pending_subagent_results: Vec<(String, String, String)>,
    held_turn: Option<HeldTurn>,

    pub approval_dialog: Option<ApprovalDialog>,
    pub approval_responder: Option<tokio::sync::oneshot::Sender<xbot::tools::ApprovalDecision>>,
    composer_history_path: PathBuf,

    pub session_title: String,
    pub session_key: String,
    pub show_session_overlay: bool,
    pub session_overlay_index: usize,
    pub session_overlay_scroll: usize,
    pub available_sessions: Vec<xbot::storage::SessionSummary>,
}

#[derive(Clone)]
pub struct ApprovalDialog {
    pub tool_name: String,
    pub path: String,
    pub diff_lines: Vec<xbot::diff::DiffLine>,
    pub source: Option<String>,
    pub selected: usize,
}

struct HeldTurn {
    content: Option<String>,
    reasoning: Option<String>,
    summary: Option<TurnSummary>,
    note: Option<String>,
}

impl App {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        model: String,
        provider: String,
        workspace: PathBuf,
        session_msg_count: usize,
        context_status: String,
        configured_subagent_model: Option<String>,
        session_title: String,
        session_key: String,
        available_sessions: Vec<xbot::storage::SessionSummary>,
    ) -> Self {
        let composer_history_path = composer_history_path(&workspace);
        let composer_history = load_composer_history(&composer_history_path);
        let mut history = Vec::new();
        if session_msg_count > 0 {
            history.push(HistoryEntry::System(format!(
                "Continuing session: {} ({} msgs, {})\n Use /session to switch sessions, /new to start fresh.",
                session_title, session_msg_count, context_status
            )));
        }
        Self {
            history,
            active: None,
            composer: ComposerState::with_history(composer_history),
            line_buffer: LineBuffer::new(),
            scroll_offset: 0,
            auto_scroll: true,
            total_lines: 0,
            show_help: false,
            show_sidebar: false,
            show_subagent_overlay: false,
            subagent_overlay_index: 0,
            subagent_overlay_scroll: 0,
            should_quit: false,
            agent_state: AgentState::Ready,
            needs_redraw: true,
            cancel_requested: false,
            pending: VecDeque::new(),
            exit_after_turn: false,
            model,
            configured_subagent_model,
            provider,
            workspace,
            session_msg_count,
            context_status,
            last_summary: None,
            animation_frame: 0,
            subagents: BTreeMap::new(),
            pending_subagent_results: Vec::new(),
            held_turn: None,
            approval_dialog: None,
            approval_responder: None,
            composer_history_path,
            session_title,
            session_key,
            show_session_overlay: false,
            session_overlay_index: 0,
            session_overlay_scroll: 0,
            available_sessions,
        }
    }

    pub fn is_busy(&self) -> bool {
        !matches!(self.agent_state, AgentState::Ready)
    }

    pub fn pop_next_prompt(&mut self) -> Option<String> {
        let queued = self.pending.pop_front()?;
        if queued.show_in_history {
            self.history.push(HistoryEntry::User(queued.prompt.clone()));
            self.auto_scroll = true;
        }
        Some(queued.prompt)
    }

    #[allow(dead_code)]
    pub fn is_waiting_subagents(&self) -> bool {
        self.agent_state == AgentState::WaitingSubagents
    }

    pub fn running_subagent_count(&self) -> usize {
        self.subagents
            .values()
            .filter(|s| s.status == SubagentStatus::Running)
            .count()
    }

    pub fn waiting_subagent_lines(&self) -> Vec<(String, SubagentStatus)> {
        self.subagents
            .values()
            .map(|s| (s.label.clone(), s.status.clone()))
            .collect()
    }

    pub fn spinner_char(&self) -> char {
        let idx = (self.animation_frame / 3) as usize % SPINNER.len();
        SPINNER[idx]
    }

    pub fn tick_animation(&mut self) {
        let prev = self.animation_frame;
        self.animation_frame = self.animation_frame.wrapping_add(1);
        let has_animation = self.is_busy() || self.running_subagent_count() > 0;
        if has_animation && (prev / 3) != (self.animation_frame / 3) {
            self.needs_redraw = true;
        }
    }

    pub fn handle_engine_event(&mut self, event: EngineEvent) {
        self.needs_redraw = true;
        match event {
            EngineEvent::StreamDelta(delta) => {
                let clean = strip_tool_markers(&strip_ansi(&delta));
                if clean.is_empty() {
                    return;
                }
                let streaming = self.active.get_or_insert_with(ActiveStreaming::default);
                streaming.push_text(&clean);
            }
            EngineEvent::ReasoningDelta(delta) => {
                let clean = strip_runtime_metadata_from_reasoning(&delta);
                if clean.is_empty() {
                    return;
                }
                let streaming = self.active.get_or_insert_with(ActiveStreaming::default);
                streaming.push_thinking(&clean);
            }
            EngineEvent::CollapseThinking => {}
            EngineEvent::ToolHint {
                tool_name,
                tool_args,
            } => {
                let name = tool_name.as_deref().unwrap_or("tool");
                let detail = tool_args
                    .as_ref()
                    .map(summarize_tool_args)
                    .unwrap_or_default();
                let diff = if matches!(name, "edit_file" | "write_file") {
                    extract_edit_diff(name, tool_args.as_ref())
                } else {
                    None
                };
                let streaming = self.active.get_or_insert_with(ActiveStreaming::default);
                let timeout_secs = if name == "wait_subagents" {
                    tool_args
                        .as_ref()
                        .and_then(|v| v.get("timeout_seconds"))
                        .and_then(|v| v.as_u64())
                        .or(Some(300))
                } else {
                    None
                };
                streaming.push_tool(ToolActivity {
                    name: name.to_string(),
                    emoji: tool_emoji(name).to_string(),
                    detail,
                    diff,
                    result_summary: None,
                    started_at: std::time::Instant::now(),
                    timeout_secs,
                });
            }
            EngineEvent::ToolResult {
                tool_name,
                success,
                summary,
            } => {
                if let Some(ref mut active) = self.active {
                    for seg in active.segments.iter_mut().rev() {
                        if let StreamSegment::Tool(activity) = seg {
                            if activity.name == tool_name && activity.result_summary.is_none() {
                                activity.result_summary = Some((success, summary.clone()));
                                break;
                            }
                        }
                    }
                }
            }
            EngineEvent::TurnComplete {
                content,
                reasoning,
                summary,
            } => {
                self.flush_line_buffer();
                let clean_content = strip_ansi(&content);
                let had_thinking = self
                    .active
                    .as_ref()
                    .is_some_and(|a| a.has_thinking_content());
                let clean_reasoning =
                    reasoning.map(|r| strip_runtime_metadata_from_reasoning(&strip_ansi(&r)));
                let effective_reasoning = if had_thinking { None } else { clean_reasoning };
                if self.running_subagent_count() > 0 {
                    let had_streamed_text =
                        self.active.as_ref().is_some_and(|a| a.has_text_content());
                    self.flush_active_to_history();
                    if !had_streamed_text && !clean_content.trim().is_empty() {
                        self.history.push(HistoryEntry::Assistant {
                            content: clean_content.clone(),
                            reasoning: effective_reasoning.clone(),
                        });
                    }
                    self.held_turn = Some(HeldTurn {
                        content: Some(clean_content),
                        reasoning: effective_reasoning,
                        summary: Some(summary),
                        note: None,
                    });
                    self.agent_state = AgentState::WaitingSubagents;
                    self.auto_scroll = true;
                } else if !self.pending_subagent_results.is_empty() {
                    let had_streamed_text =
                        self.active.as_ref().is_some_and(|a| a.has_text_content());
                    self.flush_active_to_history();
                    if !had_streamed_text && !clean_content.trim().is_empty() {
                        self.history.push(HistoryEntry::Assistant {
                            content: clean_content,
                            reasoning: effective_reasoning,
                        });
                    }
                    self.build_and_push_continuation();
                } else {
                    self.finalize_turn(
                        Some(clean_content),
                        effective_reasoning,
                        Some(summary),
                        None,
                    );
                }
            }
            EngineEvent::TurnEmpty { note, summary } => {
                self.flush_line_buffer();
                if self.running_subagent_count() > 0 {
                    self.flush_active_to_history();
                    self.held_turn = Some(HeldTurn {
                        content: None,
                        reasoning: None,
                        summary: Some(summary),
                        note: Some(note),
                    });
                    self.agent_state = AgentState::WaitingSubagents;
                    self.auto_scroll = true;
                } else if !self.pending_subagent_results.is_empty() {
                    self.flush_active_to_history();
                    self.build_and_push_continuation();
                } else {
                    self.finalize_turn(None, None, Some(summary), Some(note));
                }
            }
            EngineEvent::TurnError(err) => {
                self.flush_line_buffer();
                self.flush_active_to_history();
                self.history.push(HistoryEntry::Error(strip_ansi(&err)));
                if self.running_subagent_count() > 0 {
                    self.agent_state = AgentState::WaitingSubagents;
                    self.auto_scroll = true;
                } else if !self.pending_subagent_results.is_empty() {
                    self.build_and_push_continuation();
                } else {
                    self.agent_state = AgentState::Ready;
                    self.auto_scroll = true;
                    self.maybe_dequeue();
                }
            }
            EngineEvent::ContextUpdate(ctx) => {
                self.context_status = ctx;
            }
            EngineEvent::Summarizing => {
                self.agent_state = AgentState::Summarizing;
            }
            EngineEvent::SummarizingDone => {
                if self.agent_state == AgentState::Summarizing {
                    self.agent_state = AgentState::Working;
                }
            }
            EngineEvent::SubagentStarted {
                task_id,
                label,
                task,
                model,
            } => {
                let info = SubagentInfo {
                    task_id: task_id.clone(),
                    label: label.clone(),
                    task: task.clone(),
                    model: model.clone(),
                    status: SubagentStatus::Running,
                    actions: Vec::new(),
                    all_actions: Vec::new(),
                    result_preview: None,
                    full_result: None,
                    reasoning_chunks: Vec::new(),
                    text_chunks: Vec::new(),
                    started_at: Instant::now(),
                    finished_at: None,
                };
                self.subagents.insert(task_id.clone(), info);
                if self.active.is_some() {
                    let streaming = self.active.get_or_insert_with(ActiveStreaming::default);
                    streaming.push_subagent(task_id.clone(), label.clone());
                } else {
                    self.history.push(HistoryEntry::SubagentCard {
                        task_id,
                        label,
                        model,
                        status: SubagentStatus::Running,
                        actions: Vec::new(),
                        result_preview: None,
                    });
                }
                if !self.show_sidebar {
                    self.show_sidebar = true;
                }
            }
            EngineEvent::SubagentProgress {
                task_id,
                tool_name,
                detail,
                step,
            } => {
                let action = format!("{tool_name} {detail}");
                if let Some(info) = self.subagents.get_mut(&task_id) {
                    info.all_actions.push(format!("[{step}] {action}"));
                    info.actions.push(action.clone());
                    if info.actions.len() > 3 {
                        info.actions.remove(0);
                    }
                }
                self.update_subagent_card(&task_id, |card| {
                    if let HistoryEntry::SubagentCard { actions, .. } = card {
                        actions.push(format!("[{step}] {action}"));
                        if actions.len() > 3 {
                            actions.remove(0);
                        }
                    }
                });
            }
            EngineEvent::SubagentReasoning { task_id, content } => {
                if let Some(info) = self.subagents.get_mut(&task_id) {
                    info.reasoning_chunks.push(content);
                }
            }
            EngineEvent::SubagentTextDelta { task_id, content } => {
                if let Some(info) = self.subagents.get_mut(&task_id) {
                    info.text_chunks.push(content);
                }
            }
            EngineEvent::SubagentCompleted {
                task_id,
                label,
                result_preview,
                full_result,
            } => {
                if let Some(info) = self.subagents.get_mut(&task_id) {
                    info.status = SubagentStatus::Completed;
                    info.result_preview = Some(result_preview.clone());
                    info.full_result = Some(full_result.clone());
                    info.finished_at.get_or_insert_with(Instant::now);
                }
                self.update_subagent_card(&task_id, |card| {
                    if let HistoryEntry::SubagentCard {
                        status,
                        result_preview: rp,
                        ..
                    } = card
                    {
                        *status = SubagentStatus::Completed;
                        *rp = Some(result_preview.clone());
                    }
                });
                self.pending_subagent_results
                    .push((task_id, label, full_result));
                self.check_all_subagents_done();
            }
            EngineEvent::SubagentFailed { task_id, error, .. } => {
                if let Some(info) = self.subagents.get_mut(&task_id) {
                    info.status = SubagentStatus::Failed;
                    info.result_preview = Some(error.clone());
                    info.full_result = Some(error.clone());
                    info.finished_at.get_or_insert_with(Instant::now);
                }
                self.update_subagent_card(&task_id, |card| {
                    if let HistoryEntry::SubagentCard {
                        status,
                        result_preview,
                        ..
                    } = card
                    {
                        *status = SubagentStatus::Failed;
                        *result_preview = Some(error.clone());
                    }
                });
                self.check_all_subagents_done();
            }
            EngineEvent::SubagentCancelled { task_id } => {
                if let Some(info) = self.subagents.get_mut(&task_id) {
                    info.status = SubagentStatus::Cancelled;
                    info.finished_at.get_or_insert_with(Instant::now);
                }
                self.update_subagent_card(&task_id, |card| {
                    if let HistoryEntry::SubagentCard { status, .. } = card {
                        *status = SubagentStatus::Cancelled;
                    }
                });
                self.check_all_subagents_done();
            }
            EngineEvent::ApprovalRequest {
                tool_name,
                path,
                diff_lines,
                source,
                responder,
            } => {
                self.approval_dialog = Some(ApprovalDialog {
                    tool_name,
                    path,
                    diff_lines,
                    source,
                    selected: 0,
                });
                self.approval_responder = Some(responder);
            }
        }
    }

    fn check_all_subagents_done(&mut self) {
        if self.running_subagent_count() > 0 {
            return;
        }
        if self.agent_state != AgentState::WaitingSubagents {
            return;
        }
        if self.pending_subagent_results.is_empty() {
            if let Some(held) = self.held_turn.take() {
                self.finalize_turn(held.content, held.reasoning, held.summary, held.note);
            } else {
                self.agent_state = AgentState::Ready;
                self.auto_scroll = true;
                self.needs_redraw = true;
                self.maybe_dequeue();
            }
            return;
        }
        self.held_turn = None;
        self.build_and_push_continuation();
    }

    fn build_and_push_continuation(&mut self) {
        let results = std::mem::take(&mut self.pending_subagent_results);
        if results.is_empty() {
            self.agent_state = AgentState::Ready;
            self.auto_scroll = true;
            self.needs_redraw = true;
            return;
        }
        let mut continuation = String::new();
        for (task_id, label, result) in &results {
            if !continuation.is_empty() {
                continuation.push_str("\n\n---\n\n");
            }
            let task = self
                .subagents
                .get(task_id)
                .map(|info| info.task.as_str())
                .unwrap_or("unknown task");
            continuation.push_str(&format!(
                "[Subagent '{label}' completed]\n\nTask: {task}\n\nResult:\n{result}\n\n\
                 Continue the task using these results. \
                 Summarize for the user what was accomplished."
            ));
        }
        self.agent_state = AgentState::Ready;
        self.auto_scroll = true;
        self.needs_redraw = true;
        self.pending.push_back(QueuedPrompt::internal(continuation));
    }

    fn update_subagent_card(&mut self, task_id: &str, f: impl Fn(&mut HistoryEntry)) {
        for entry in self.history.iter_mut().rev() {
            if let HistoryEntry::SubagentCard { task_id: id, .. } = entry {
                if id == task_id {
                    f(entry);
                    return;
                }
            }
        }
    }

    fn flush_line_buffer(&mut self) {
        let remaining = self.line_buffer.flush();
        if !remaining.is_empty() {
            let clean = strip_ansi(&remaining);
            let streaming = self.active.get_or_insert_with(ActiveStreaming::default);
            streaming.push_text(&clean);
        }
    }

    fn finalize_turn(
        &mut self,
        content: Option<String>,
        reasoning: Option<String>,
        summary: Option<TurnSummary>,
        note: Option<String>,
    ) {
        if let Some(streaming) = self.active.take() {
            let mut accumulated_text = String::new();
            let mut had_thinking = false;
            for seg in streaming.segments {
                match seg {
                    StreamSegment::Text(text) => {
                        accumulated_text.push_str(&text);
                    }
                    StreamSegment::Thinking(text) => {
                        had_thinking = true;
                        if !accumulated_text.trim().is_empty() {
                            self.history.push(HistoryEntry::Assistant {
                                content: std::mem::take(&mut accumulated_text),
                                reasoning: None,
                            });
                        } else {
                            accumulated_text.clear();
                        }
                        self.history.push(HistoryEntry::Thinking(text));
                    }
                    StreamSegment::Tool(tool) => {
                        if !accumulated_text.trim().is_empty() {
                            self.history.push(HistoryEntry::Assistant {
                                content: std::mem::take(&mut accumulated_text),
                                reasoning: None,
                            });
                        } else {
                            accumulated_text.clear();
                        }
                        self.history.push(HistoryEntry::ToolCall {
                            name: tool.name,
                            emoji: tool.emoji,
                            detail: tool.detail,
                            diff: tool.diff,
                            result_summary: tool.result_summary,
                        });
                    }
                    StreamSegment::Subagent { task_id, label } => {
                        if !accumulated_text.trim().is_empty() {
                            self.history.push(HistoryEntry::Assistant {
                                content: std::mem::take(&mut accumulated_text),
                                reasoning: None,
                            });
                        } else {
                            accumulated_text.clear();
                        }
                        let status = self
                            .subagents
                            .get(&task_id)
                            .map(|i| i.status.clone())
                            .unwrap_or(SubagentStatus::Running);
                        let actions = self
                            .subagents
                            .get(&task_id)
                            .map(|i| i.actions.clone())
                            .unwrap_or_default();
                        let preview = self
                            .subagents
                            .get(&task_id)
                            .and_then(|i| i.result_preview.clone());
                        let model = self
                            .subagents
                            .get(&task_id)
                            .map(|i| i.model.clone())
                            .unwrap_or_default();
                        self.history.push(HistoryEntry::SubagentCard {
                            task_id,
                            label,
                            model,
                            status,
                            actions,
                            result_preview: preview,
                        });
                    }
                }
            }

            let final_text = match content {
                Some(final_content) if !final_content.trim().is_empty() => final_content,
                _ => accumulated_text,
            };

            let effective_reasoning = if had_thinking { None } else { reasoning };

            if !final_text.trim().is_empty() {
                self.history.push(HistoryEntry::Assistant {
                    content: final_text,
                    reasoning: effective_reasoning,
                });
            } else if let Some(note) = note {
                self.history.push(HistoryEntry::System(format!("· {note}")));
            }
        } else {
            let final_text = content.unwrap_or_default();
            if !final_text.trim().is_empty() {
                self.history.push(HistoryEntry::Assistant {
                    content: final_text,
                    reasoning,
                });
            } else if let Some(note) = note {
                self.history.push(HistoryEntry::System(format!("· {note}")));
            }
        }

        if let Some(summary) = summary.clone() {
            self.history.push(HistoryEntry::Separator { summary });
        }
        self.last_summary = summary;
        self.agent_state = AgentState::Ready;
        self.auto_scroll = true;
        self.maybe_dequeue();
    }

    fn flush_active_to_history(&mut self) {
        if let Some(streaming) = self.active.take() {
            for seg in streaming.segments {
                match seg {
                    StreamSegment::Text(text) => {
                        if !text.trim().is_empty() {
                            self.history.push(HistoryEntry::Assistant {
                                content: text,
                                reasoning: None,
                            });
                        }
                    }
                    StreamSegment::Thinking(text) => {
                        if !text.trim().is_empty() {
                            self.history.push(HistoryEntry::Thinking(text));
                        }
                    }
                    StreamSegment::Tool(tool) => {
                        self.history.push(HistoryEntry::ToolCall {
                            name: tool.name,
                            emoji: tool.emoji,
                            detail: tool.detail,
                            diff: tool.diff,
                            result_summary: tool.result_summary,
                        });
                    }
                    StreamSegment::Subagent { task_id, label } => {
                        let status = self
                            .subagents
                            .get(&task_id)
                            .map(|i| i.status.clone())
                            .unwrap_or(SubagentStatus::Running);
                        let actions = self
                            .subagents
                            .get(&task_id)
                            .map(|i| i.actions.clone())
                            .unwrap_or_default();
                        let preview = self
                            .subagents
                            .get(&task_id)
                            .and_then(|i| i.result_preview.clone());
                        let model = self
                            .subagents
                            .get(&task_id)
                            .map(|i| i.model.clone())
                            .unwrap_or_default();
                        self.history.push(HistoryEntry::SubagentCard {
                            task_id,
                            label,
                            model,
                            status,
                            actions,
                            result_preview: preview,
                        });
                    }
                }
            }
        }
    }

    pub fn flush_active_as_cancelled(&mut self) {
        self.flush_line_buffer();
        self.flush_active_to_history();
        self.history
            .push(HistoryEntry::System("⏹ turn cancelled".into()));
        self.auto_scroll = true;
    }

    fn reset_session_view(&mut self) {
        self.history.clear();
        self.active = None;
        self.line_buffer = LineBuffer::new();
        self.scroll_offset = 0;
        self.auto_scroll = true;
        self.last_summary = None;
        self.session_msg_count = 0;
        self.subagents.clear();
        self.pending_subagent_results.clear();
        self.held_turn = None;
        self.show_sidebar = false;
        self.show_subagent_overlay = false;
        self.subagent_overlay_index = 0;
        self.subagent_overlay_scroll = 0;
        self.show_session_overlay = false;
    }

    fn switch_to_session(&mut self, new_key: String, new_title: String, new_msg_count: usize) {
        self.reset_session_view();
        self.pending.clear();
        self.session_key = new_key;
        self.session_title = new_title.clone();
        self.session_msg_count = new_msg_count;
        self.pending.push_back(QueuedPrompt::internal(format!(
            "/switch {}",
            self.session_key
        )));
        if new_msg_count > 0 {
            self.history.push(HistoryEntry::System(format!(
                "Switched to session: {} ({} msgs)\nUse /session to switch, /new for fresh.",
                new_title, new_msg_count
            )));
        } else {
            self.history.push(HistoryEntry::System(format!(
                "Switched to session: {} (empty)\nUse /session to switch, /new for fresh.",
                new_title
            )));
        }
        self.auto_scroll = true;
    }

    pub fn session_key(&self) -> &str {
        &self.session_key
    }

    pub fn refresh_available_sessions(&mut self, summaries: Vec<xbot::storage::SessionSummary>) {
        self.available_sessions = summaries;
    }

    fn maybe_dequeue(&mut self) {
        if self.exit_after_turn {
            self.should_quit = true;
        }
    }

    pub fn handle_crossterm_event(&mut self, event: Event) {
        match event {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                self.handle_key(key);
            }
            Event::Mouse(mouse) => {
                if matches!(mouse.kind, MouseEventKind::ScrollUp) {
                    self.scroll_up(3);
                } else if matches!(mouse.kind, MouseEventKind::ScrollDown) {
                    self.scroll_down(3);
                }
            }
            Event::Paste(text) => {
                if !self.show_help {
                    self.composer.insert_paste(&text);
                    self.needs_redraw = true;
                }
            }
            Event::Resize(_, _) => {
                self.needs_redraw = true;
            }
            _ => {}
        }
    }

    pub fn send_approval_decision(&mut self) {
        let dialog = match self.approval_dialog.take() {
            Some(d) => d,
            None => return,
        };
        let responder = match self.approval_responder.take() {
            Some(r) => r,
            None => return,
        };
        let decision = match dialog.selected {
            0 => xbot::tools::ApprovalDecision::AllowOnce,
            1 => xbot::tools::ApprovalDecision::AlwaysAllow,
            _ => xbot::tools::ApprovalDecision::Deny,
        };
        let _ = responder.send(decision);
    }

    fn handle_key(&mut self, key: KeyEvent) {
        self.needs_redraw = true;

        if self.approval_dialog.is_some() {
            match key.code {
                KeyCode::Left => {
                    if let Some(ref mut d) = self.approval_dialog {
                        d.selected = d.selected.saturating_sub(1);
                    }
                }
                KeyCode::Right => {
                    if let Some(ref mut d) = self.approval_dialog {
                        d.selected = (d.selected + 1).min(2);
                    }
                }
                KeyCode::Enter => {
                    self.send_approval_decision();
                }
                KeyCode::Char('1') => {
                    if let Some(ref mut d) = self.approval_dialog {
                        d.selected = 0;
                    }
                }
                KeyCode::Char('2') => {
                    if let Some(ref mut d) = self.approval_dialog {
                        d.selected = 1;
                    }
                }
                KeyCode::Char('3') => {
                    if let Some(ref mut d) = self.approval_dialog {
                        d.selected = 2;
                    }
                }
                KeyCode::Esc => {
                    if let Some(ref mut d) = self.approval_dialog {
                        d.selected = 2; // Deny
                    }
                }
                _ => {}
            }
            return;
        }

        if self.show_help {
            match key.code {
                KeyCode::Esc | KeyCode::F(1) | KeyCode::Char('q') | KeyCode::Char('?') => {
                    self.show_help = false;
                }
                KeyCode::Up | KeyCode::Char('k') => self.scroll_up(1),
                KeyCode::Down | KeyCode::Char('j') => self.scroll_down(1),
                KeyCode::PageUp => self.scroll_up(20),
                KeyCode::PageDown => self.scroll_down(20),
                _ => {}
            }
            return;
        }

        if self.show_session_overlay {
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') => {
                    self.show_session_overlay = false;
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    if self.session_overlay_index > 0 {
                        self.session_overlay_index -= 1;
                    }
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    let count = self.available_sessions.len();
                    if count > 0 && self.session_overlay_index < count - 1 {
                        self.session_overlay_index += 1;
                    }
                }
                KeyCode::Enter => {
                    if let Some(selected) = self.available_sessions.get(self.session_overlay_index)
                    {
                        let new_key = selected.key.clone();
                        let new_title = selected.title.clone();
                        let new_msg_count = selected.message_count;
                        self.show_session_overlay = false;
                        if new_key != self.session_key {
                            self.switch_to_session(new_key, new_title, new_msg_count);
                        }
                    }
                }
                _ => {}
            }
            return;
        }

        if self.show_subagent_overlay {
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') => {
                    self.show_subagent_overlay = false;
                }
                KeyCode::Char('4') if key.modifiers.contains(KeyModifiers::ALT) => {
                    self.show_subagent_overlay = false;
                }
                KeyCode::Left | KeyCode::Char('h') => {
                    if self.subagent_overlay_index > 0 {
                        self.subagent_overlay_index -= 1;
                        self.subagent_overlay_scroll = 0;
                    }
                }
                KeyCode::Right | KeyCode::Char('l') => {
                    let count = self.subagents.len();
                    if count > 0 && self.subagent_overlay_index < count - 1 {
                        self.subagent_overlay_index += 1;
                        self.subagent_overlay_scroll = 0;
                    }
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.subagent_overlay_scroll = self.subagent_overlay_scroll.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.subagent_overlay_scroll += 1;
                }
                KeyCode::PageUp => {
                    self.subagent_overlay_scroll = self.subagent_overlay_scroll.saturating_sub(20);
                }
                KeyCode::PageDown => {
                    self.subagent_overlay_scroll += 20;
                }
                _ => {}
            }
            return;
        }

        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if self.is_busy() {
                    self.cancel_requested = true;
                } else if self.composer.input.trim().is_empty() {
                    self.should_quit = true;
                } else {
                    self.composer.clear_line();
                }
            }
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if self.composer.input.is_empty() && !self.is_busy() {
                    self.should_quit = true;
                }
            }
            KeyCode::Char('4') if key.modifiers.contains(KeyModifiers::ALT) => {
                if self.show_subagent_overlay {
                    self.show_subagent_overlay = false;
                    self.show_sidebar = false;
                } else if self.show_sidebar {
                    if !self.subagents.is_empty() {
                        self.show_subagent_overlay = true;
                        self.subagent_overlay_scroll = 0;
                    } else {
                        self.show_sidebar = false;
                    }
                } else {
                    self.show_sidebar = true;
                }
            }
            KeyCode::Enter => {
                if key.modifiers.contains(KeyModifiers::ALT)
                    || key.modifiers.contains(KeyModifiers::SHIFT)
                {
                    self.composer.insert_newline();
                    return;
                }
                let text = self.composer.input.trim().to_string();
                if text.is_empty() {
                    return;
                }
                if let Some(local) = parse_local_command(&text) {
                    self.handle_local_command(local);
                    return;
                }
                let prompt = self.take_composer_input();
                self.pending.push_back(QueuedPrompt::user(prompt));
            }
            KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.composer.insert_newline();
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.composer.clear_line();
            }
            KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.composer.delete_word_back();
            }
            KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.composer.move_home();
            }
            KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.composer.move_end();
            }
            KeyCode::Backspace => self.composer.backspace(),
            KeyCode::Delete => self.composer.delete(),
            KeyCode::Left => self.composer.move_left(),
            KeyCode::Right => self.composer.move_right(),
            KeyCode::Home => self.composer.move_home(),
            KeyCode::End => self.composer.move_end(),
            KeyCode::Up => {
                if key.modifiers.contains(KeyModifiers::SHIFT) {
                    if !self.composer.history.is_empty() && !self.composer.input.contains('\n') {
                        self.composer.history_up();
                    }
                } else {
                    self.scroll_up(1);
                }
            }
            KeyCode::Down => {
                if key.modifiers.contains(KeyModifiers::SHIFT) {
                    if self.composer.history_index.is_some() {
                        self.composer.history_down();
                    }
                } else {
                    self.scroll_down(1);
                }
            }
            KeyCode::PageUp => self.scroll_up(20),
            KeyCode::PageDown => self.scroll_down(20),
            KeyCode::F(1) | KeyCode::F(12) => self.show_help = !self.show_help,
            KeyCode::Char('/') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.show_help = !self.show_help;
            }
            KeyCode::Char('?')
                if self.composer.input.is_empty()
                    && !key.modifiers.contains(KeyModifiers::SHIFT) =>
            {
                self.show_help = !self.show_help;
            }
            KeyCode::Esc => {
                if !self.composer.input.is_empty() {
                    self.composer.clear_line();
                }
            }
            KeyCode::Char(ch) => self.composer.insert_char(ch),
            _ => {}
        }
    }

    fn handle_local_command(&mut self, cmd: LocalCommand) {
        self.take_composer_input();
        match cmd {
            LocalCommand::Help => self.show_help = true,
            LocalCommand::Exit => {
                if self.is_busy() {
                    self.exit_after_turn = true;
                    self.pending.clear();
                    self.history.push(HistoryEntry::System(
                        "⏳ exit requested · finishing current turn…".into(),
                    ));
                } else {
                    self.should_quit = true;
                }
            }
            LocalCommand::Stop => {
                if self.is_busy() {
                    self.cancel_requested = true;
                }
            }
            LocalCommand::Clear => {
                self.reset_session_view();
                self.pending.clear();
                self.pending
                    .push_back(QueuedPrompt::internal("/new".to_string()));
            }
            LocalCommand::Agents => {
                self.show_sidebar = !self.show_sidebar;
            }
            LocalCommand::Sessions => {
                if self.available_sessions.len() > 1 {
                    self.show_session_overlay = true;
                    self.session_overlay_index = self
                        .available_sessions
                        .iter()
                        .position(|s| s.key == self.session_key)
                        .unwrap_or(0);
                    self.session_overlay_scroll = 0;
                } else {
                    self.history.push(HistoryEntry::System(
                        "No other sessions available. Use /new to create one.".into(),
                    ));
                    self.auto_scroll = true;
                }
            }
            LocalCommand::Agent(text) => {
                self.pending.push_back(QueuedPrompt::user(text));
            }
        }
    }

    pub fn scroll_up(&mut self, lines: usize) {
        self.scroll_offset = self.scroll_offset.saturating_sub(lines);
        self.auto_scroll = false;
        self.needs_redraw = true;
    }

    pub fn scroll_down(&mut self, lines: usize) {
        self.scroll_offset = self.scroll_offset.saturating_add(lines);
        self.auto_scroll = false;
        self.needs_redraw = true;
    }

    pub fn clamp_scroll(&mut self, visible_height: usize) {
        if self.total_lines <= visible_height {
            self.scroll_offset = 0;
            return;
        }
        let max = self.total_lines.saturating_sub(visible_height);
        if self.auto_scroll {
            self.scroll_offset = max;
        } else if self.scroll_offset > max {
            self.scroll_offset = max;
        }
        if self.scroll_offset >= max {
            self.auto_scroll = true;
        }
    }

    pub fn status_line(&self) -> String {
        let state = match self.agent_state {
            AgentState::Ready => "ready",
            AgentState::Working => "working",
            AgentState::WaitingSubagents => "waiting agents",
            AgentState::Summarizing => "summarizing",
        };
        let mut parts = vec![state.to_string()];
        let running = self.running_subagent_count();
        if running > 0 {
            parts.push(format!(
                "{running} agent{}",
                if running == 1 { "" } else { "s" }
            ));
        }
        if let Some(ref s) = self.last_summary {
            if s.prompt_tokens > 0 || s.completion_tokens > 0 {
                let cache_hint = if s.cached_tokens > 0 && s.prompt_tokens > 0 {
                    let pct = (s.cached_tokens * 100) / s.prompt_tokens;
                    format!("({}% cached) ", pct)
                } else {
                    String::new()
                };
                parts.push(format!(
                    "↑{} {}↓{}",
                    s.prompt_tokens, cache_hint, s.completion_tokens
                ));
            }
            parts.push(format_elapsed(s.elapsed));
        }
        parts.join(" · ")
    }

    fn take_composer_input(&mut self) -> String {
        let text = self.composer.take_input();
        save_composer_history(&self.composer_history_path, &self.composer.history);
        text
    }
}

fn composer_history_path(workspace: &std::path::Path) -> PathBuf {
    workspace_state_dir(workspace).join(COMPOSER_HISTORY_FILE)
}

fn load_composer_history(path: &std::path::Path) -> Vec<String> {
    let Ok(content) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(history) = serde_json::from_str::<Vec<String>>(&content) else {
        return Vec::new();
    };
    trim_composer_history(history)
}

fn save_composer_history(path: &std::path::Path, history: &[String]) {
    let Some(parent) = path.parent() else {
        return;
    };
    if ensure_dir(parent).is_err() {
        return;
    }
    let Ok(content) = serde_json::to_string_pretty(history) else {
        return;
    };
    let _ = fs::write(path, format!("{content}\n"));
}

fn trim_composer_history(history: Vec<String>) -> Vec<String> {
    let mut history = history
        .into_iter()
        .filter(|entry| !entry.trim().is_empty())
        .collect::<Vec<_>>();
    trim_composer_history_in_place(&mut history);
    history
}

fn trim_composer_history_in_place(history: &mut Vec<String>) {
    if history.len() > COMPOSER_HISTORY_LIMIT {
        history.drain(..history.len() - COMPOSER_HISTORY_LIMIT);
    }
}

enum LocalCommand {
    Help,
    Exit,
    Stop,
    Clear,
    Agents,
    Sessions,
    Agent(String),
}

fn parse_local_command(input: &str) -> Option<LocalCommand> {
    let t = input.trim();
    let lower = t.to_lowercase();
    match lower.as_str() {
        "/help" | "help" | "?" => Some(LocalCommand::Help),
        "/exit" | "/quit" | "exit" | "quit" => Some(LocalCommand::Exit),
        "/stop" | "stop" | "[stop]" => Some(LocalCommand::Stop),
        "/clear" | "clear" | "/new" | "new" => Some(LocalCommand::Clear),
        "/agents" => Some(LocalCommand::Agents),
        "/session" | "/sessions" => Some(LocalCommand::Sessions),
        _ => {
            if lower.starts_with("/memorize")
                || lower.starts_with("/model")
                || lower.starts_with("/status")
            {
                Some(LocalCommand::Agent(t.to_string()))
            } else {
                None
            }
        }
    }
}

pub fn strip_ansi(text: &str) -> String {
    let bytes = text.as_bytes();
    let len = bytes.len();
    if !bytes.contains(&0x1b) {
        return text.to_string();
    }
    let mut out = String::with_capacity(len);
    let mut i = 0;
    while i < len {
        if bytes[i] == 0x1b {
            i += 1;
            if i < len && bytes[i] == b'[' {
                i += 1;
                while i < len && !(bytes[i].is_ascii_alphabetic() || bytes[i] == b'm') {
                    i += 1;
                }
                if i < len {
                    i += 1;
                }
            } else if i < len && bytes[i] == b']' {
                i += 1;
                while i < len {
                    if bytes[i] == 0x07 {
                        i += 1;
                        break;
                    }
                    if bytes[i] == 0x1b && i + 1 < len && bytes[i + 1] == b'\\' {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
            }
        } else {
            let ch = text[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

fn strip_tool_markers(text: &str) -> String {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(
            r"(?x)
            </?tool_call> |
            </?tool_response> |
            </?tool_use> |
            </?function_call> |
            </?invoke[^>]*> |
            </?parameter[^>]*> |
            <\|/?tool_call\|?> |
            <\|/?endoftoolcall\|?> |
            <\|/?tool_response\|?>
            ",
        )
        .unwrap()
    });
    let result = re.replace_all(text, "");
    if result.len() == text.len() {
        text.to_string()
    } else {
        result.into_owned()
    }
}

fn strip_runtime_metadata_from_reasoning(text: &str) -> String {
    text.lines()
        .filter(|line| {
            let trimmed = line.trim();
            !trimmed.starts_with(xbot::engine::ContextBuilder::RUNTIME_CONTEXT_TAG)
                && !trimmed.starts_with("Current Time:")
                && !trimmed.starts_with("Channel:")
                && !trimmed.starts_with("Chat ID:")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn extract_edit_diff(tool_name: &str, args: Option<&Value>) -> Option<EditDiff> {
    let obj = args?.as_object()?;
    let path = obj
        .get("target_file")
        .or_else(|| obj.get("path"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();

    match tool_name {
        "edit_file" => {
            let old = obj
                .get("old_text")
                .or_else(|| obj.get("old_string"))
                .and_then(|v| v.as_str())?;
            let new = obj
                .get("new_text")
                .or_else(|| obj.get("new_string"))
                .and_then(|v| v.as_str())?;
            if old.is_empty() && new.is_empty() {
                return None;
            }
            let computed = xbot::diff::compute_diff(old, new);
            Some(EditDiff {
                path,
                lines: computed.lines,
            })
        }
        "write_file" => {
            let content = obj.get("content").and_then(|v| v.as_str())?;
            if content.is_empty() {
                return None;
            }
            let computed = xbot::diff::compute_write_diff(content);
            Some(EditDiff {
                path,
                lines: computed.lines,
            })
        }
        _ => None,
    }
}

fn summarize_tool_args(args: &Value) -> String {
    match args {
        Value::Object(map) => {
            let preferred = [
                "path",
                "target_file",
                "file",
                "command",
                "cmd",
                "url",
                "query",
                "pattern",
                "task",
            ];
            let mut parts = Vec::new();
            for key in preferred {
                if let Some(val) = map.get(key) {
                    let s = summarize_value(val);
                    if !s.is_empty() {
                        parts.push(format!("{key}={s}"));
                    }
                }
            }
            if parts.is_empty() {
                map.iter()
                    .take(2)
                    .filter_map(|(k, v)| {
                        let s = summarize_value(v);
                        (!s.is_empty()).then(|| format!("{k}={s}"))
                    })
                    .collect::<Vec<_>>()
                    .join(" · ")
            } else {
                parts.join(" · ")
            }
        }
        _ => summarize_value(args),
    }
}

fn summarize_value(val: &Value) -> String {
    match val {
        Value::String(s) => truncate_mid(s.trim(), 48),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Array(a) => {
            let items: Vec<_> = a.iter().take(3).map(summarize_value).collect();
            if a.len() > 3 {
                format!("{} …", items.join(", "))
            } else {
                items.join(", ")
            }
        }
        Value::Null => String::new(),
        Value::Object(_) => "{…}".to_string(),
    }
}

fn truncate_mid(text: &str, max: usize) -> String {
    let len = text.chars().count();
    if len <= max {
        return text.to_string();
    }
    let head = max / 2 - 1;
    let tail = max - head - 1;
    let start: String = text.chars().take(head).collect();
    let end: String = text
        .chars()
        .rev()
        .take(tail)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    format!("{start}…{end}")
}

pub fn format_elapsed(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        format!("{s}s")
    } else {
        format!("{}m {}s", s / 60, s % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::ui::compute_cursor_position;
    use tempfile::tempdir;

    #[test]
    fn composer_basic_editing() {
        let mut c = ComposerState::new();
        c.insert_char('h');
        c.insert_char('i');
        assert_eq!(c.input, "hi");
        assert_eq!(c.cursor, 2);
        c.backspace();
        assert_eq!(c.input, "h");
        assert_eq!(c.cursor, 1);
    }

    #[test]
    fn composer_multiline() {
        let mut c = ComposerState::new();
        c.insert_char('a');
        c.insert_newline();
        c.insert_char('b');
        assert_eq!(c.input, "a\nb");
        assert_eq!(c.cursor, 3);
    }

    #[test]
    fn composer_paste_normalizes_crlf_newlines() {
        let mut c = ComposerState::new();
        c.insert_paste("a\r\nb\rc");
        assert_eq!(c.input, "a\nb\nc");
        assert_eq!(c.cursor, 5);
    }

    fn test_app() -> App {
        App::new(
            "test".into(),
            "test".into(),
            PathBuf::from("/tmp"),
            0,
            "0/1000".into(),
            None,
            String::new(),
            "test:key".into(),
            Vec::new(),
        )
    }

    #[test]
    fn enter_while_busy_queues_without_history_entry() {
        let mut app = test_app();
        app.agent_state = AgentState::Working;
        app.composer.input = "follow up".into();
        app.composer.cursor = app.composer.input.chars().count();

        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(app.history.is_empty());
        assert_eq!(app.pending.len(), 1);
        let queued = app.pending.front().unwrap();
        assert_eq!(queued.prompt, "follow up");
        assert!(queued.show_in_history);
    }

    #[test]
    fn pop_next_prompt_inserts_user_prompt_at_start_of_turn() {
        let mut app = test_app();
        app.pending
            .push_back(QueuedPrompt::user("queued user".into()));

        assert_eq!(app.pop_next_prompt().as_deref(), Some("queued user"));
        assert_eq!(app.pending.len(), 0);
        assert!(matches!(
            app.history.as_slice(),
            [HistoryEntry::User(text)] if text == "queued user"
        ));
    }

    #[test]
    fn pop_next_prompt_hides_internal_prompt_from_history() {
        let mut app = test_app();
        app.pending.push_back(QueuedPrompt::internal("/new".into()));

        assert_eq!(app.pop_next_prompt().as_deref(), Some("/new"));
        assert!(app.history.is_empty());
    }

    #[test]
    fn compute_cursor_position_multiline_wrap() {
        // Test cursor position with word wrapping
        let input = "hello world";
        let width = 5;

        // Position at start
        let (x, y) = compute_cursor_position(input, 0, width);
        assert_eq!((x, y), (0, 0));

        // Position after "hello" (should wrap)
        let (x, y) = compute_cursor_position(input, 5, width);
        assert_eq!((x, y), (0, 1));

        // Position at end
        let (x, y) = compute_cursor_position(input, 11, width);
        assert_eq!((x, y), (1, 2));
    }

    #[test]
    fn compute_cursor_position_with_newlines_and_wrap() {
        // Multi-line with word wrapping
        let input = "hello\nworld";
        let width = 5;

        // Position at newline
        let (x, y) = compute_cursor_position(input, 5, width);
        assert_eq!((x, y), (0, 1));

        // Position at end
        let (x, y) = compute_cursor_position(input, 11, width);
        assert_eq!((x, y), (0, 2));
    }

    #[test]
    fn composer_history() {
        let mut c = ComposerState::new();
        c.input = "first".into();
        c.cursor = 5;
        c.take_input();
        c.input = "second".into();
        c.cursor = 6;
        c.take_input();
        assert_eq!(c.history.len(), 2);
        c.history_up();
        assert_eq!(c.input, "second");
        c.history_up();
        assert_eq!(c.input, "first");
        c.history_down();
        assert_eq!(c.input, "second");
    }

    #[test]
    fn composer_history_keeps_last_ten_inputs() {
        let mut c = ComposerState::new();
        for i in 0..12 {
            c.input = format!("prompt {i}");
            c.cursor = c.input.chars().count();
            c.take_input();
        }

        assert_eq!(c.history.len(), 10);
        assert_eq!(c.history.first().map(String::as_str), Some("prompt 2"));
        assert_eq!(c.history.last().map(String::as_str), Some("prompt 11"));
    }

    #[test]
    fn app_persists_composer_history_across_instances() {
        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        let mut app = App::new(
            "main".into(),
            "test".into(),
            workspace.clone(),
            0,
            "0/1000".into(),
            None,
            String::new(),
            "test:key".into(),
            Vec::new(),
        );
        app.composer.input = "remember me".into();
        app.composer.cursor = app.composer.input.chars().count();

        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        let app = App::new(
            "main".into(),
            "test".into(),
            workspace,
            0,
            "0/1000".into(),
            None,
            String::new(),
            "test:key".into(),
            Vec::new(),
        );
        assert_eq!(app.composer.history, vec!["remember me"]);
    }

    #[test]
    fn app_loads_only_last_ten_persisted_history_entries() {
        let dir = tempdir().unwrap();
        let history_path = composer_history_path(dir.path());
        ensure_dir(history_path.parent().unwrap()).unwrap();
        let history = (0..12).map(|i| format!("prompt {i}")).collect::<Vec<_>>();
        fs::write(&history_path, serde_json::to_string(&history).unwrap()).unwrap();

        let app = App::new(
            "main".into(),
            "test".into(),
            dir.path().to_path_buf(),
            0,
            "0/1000".into(),
            None,
            String::new(),
            "test:key".into(),
            Vec::new(),
        );

        assert_eq!(app.composer.history.len(), 10);
        assert_eq!(
            app.composer.history.first().map(String::as_str),
            Some("prompt 2")
        );
        assert_eq!(
            app.composer.history.last().map(String::as_str),
            Some("prompt 11")
        );
    }

    #[test]
    fn local_commands_parsed() {
        assert!(matches!(
            parse_local_command("/help"),
            Some(LocalCommand::Help)
        ));
        assert!(matches!(
            parse_local_command("/exit"),
            Some(LocalCommand::Exit)
        ));
        assert!(matches!(
            parse_local_command("/stop"),
            Some(LocalCommand::Stop)
        ));
        assert!(matches!(
            parse_local_command("/clear"),
            Some(LocalCommand::Clear)
        ));
        assert!(matches!(
            parse_local_command("/new"),
            Some(LocalCommand::Clear)
        ));
        assert!(matches!(
            parse_local_command("/agents"),
            Some(LocalCommand::Agents)
        ));
        assert!(matches!(
            parse_local_command("/session"),
            Some(LocalCommand::Sessions)
        ));
        assert!(matches!(
            parse_local_command("/sessions"),
            Some(LocalCommand::Sessions)
        ));
        assert!(matches!(
            parse_local_command("/model gpt-4"),
            Some(LocalCommand::Agent(_))
        ));
        assert!(parse_local_command("hello world").is_none());
    }

    #[test]
    fn session_overlay_opens_only_with_multiple_sessions() {
        let mut app = test_app();
        app.handle_local_command(LocalCommand::Sessions);
        assert!(!app.show_session_overlay);
        assert_eq!(app.history.len(), 1);

        app.available_sessions = vec![
            xbot::storage::SessionSummary {
                key: "cli:a:1".into(),
                updated_at: "2026-01-01T00:00:00Z".into(),
                message_count: 5,
                last_consolidated: 0,
                title: "Fix bugs".into(),
                estimated_tokens: 2000,
            },
            xbot::storage::SessionSummary {
                key: "cli:b:2".into(),
                updated_at: "2026-01-02T00:00:00Z".into(),
                message_count: 3,
                last_consolidated: 0,
                title: "Add tests".into(),
                estimated_tokens: 1000,
            },
        ];
        app.handle_local_command(LocalCommand::Sessions);
        assert!(app.show_session_overlay);
    }

    #[test]
    fn session_switch_resets_state_and_queues_switch_command() {
        let mut app = test_app();
        app.history.push(HistoryEntry::User("old prompt".into()));
        app.session_msg_count = 3;
        app.switch_to_session("cli:new:key".into(), "New session".into(), 5);

        assert_eq!(app.session_key, "cli:new:key");
        assert_eq!(app.session_title, "New session");
        assert_eq!(app.session_msg_count, 5);
        assert_eq!(app.pending.len(), 1);
        let queued = app.pending.front().unwrap();
        assert!(queued.prompt.starts_with("/switch "));
        assert!(!queued.show_in_history);
    }

    #[test]
    fn line_buffer_gates_until_newline() {
        let mut lb = LineBuffer::new();
        lb.push("hello ");
        assert_eq!(lb.take_committable(), "");
        lb.push("world\nfoo");
        assert_eq!(lb.take_committable(), "hello world\n");
        assert_eq!(lb.flush(), "foo");
    }

    #[test]
    fn line_buffer_multiple_newlines() {
        let mut lb = LineBuffer::new();
        lb.push("a\nb\nc\npartial");
        let committed = lb.take_committable();
        assert_eq!(committed, "a\nb\nc\n");
        assert_eq!(lb.flush(), "partial");
    }

    #[test]
    fn edit_diff_extraction() {
        let args = serde_json::json!({
            "target_file": "src/main.rs",
            "old_text": "let x = 1;",
            "new_text": "let x = 2;\nlet y = 3;"
        });
        let diff = extract_edit_diff("edit_file", Some(&args)).unwrap();
        assert_eq!(diff.path, "src/main.rs");
        assert!(
            diff.lines
                .iter()
                .any(|l| l.kind == xbot::diff::DiffKind::Removed)
        );
        assert!(
            diff.lines
                .iter()
                .any(|l| l.kind == xbot::diff::DiffKind::Added)
        );
    }

    #[test]
    fn strip_ansi_removes_escape_codes() {
        assert_eq!(strip_ansi("hello \x1b[1mworld\x1b[0m"), "hello world");
        assert_eq!(strip_ansi("no escapes"), "no escapes");
        assert_eq!(strip_ansi("\x1b[31mred\x1b[0m"), "red");
    }

    #[test]
    fn strip_runtime_metadata_from_reasoning_hides_tui_noise() {
        let reasoning = format!(
            "I need context.\n{}\nCurrent Time: now\nChannel: cli\nChat ID: xbot\nNow solve.",
            xbot::engine::ContextBuilder::RUNTIME_CONTEXT_TAG
        );

        assert_eq!(
            strip_runtime_metadata_from_reasoning(&reasoning),
            "I need context.\nNow solve."
        );
    }

    #[test]
    fn stream_segments_maintain_order() {
        let mut active = ActiveStreaming::default();
        active.push_text("hello ");
        active.push_tool(ToolActivity {
            name: "read_file".into(),
            emoji: "📖".into(),
            detail: "path=foo".into(),
            diff: None,
            result_summary: None,
            started_at: std::time::Instant::now(),
            timeout_secs: None,
        });
        active.push_text("world");
        assert_eq!(active.segments.len(), 3);
        assert!(matches!(&active.segments[0], StreamSegment::Text(s) if s == "hello "));
        assert!(matches!(&active.segments[1], StreamSegment::Tool(_)));
        assert!(matches!(&active.segments[2], StreamSegment::Text(s) if s == "world"));
    }

    #[test]
    fn stream_delta_without_newline_stays_before_later_tool() {
        let mut app = test_app();

        app.handle_engine_event(EngineEvent::StreamDelta("partial answer".into()));
        app.handle_engine_event(EngineEvent::ToolHint {
            tool_name: Some("read_file".into()),
            tool_args: Some(serde_json::json!({ "path": "src/main.rs" })),
        });

        let active = app.active.as_ref().unwrap();
        assert_eq!(active.segments.len(), 2);
        assert!(matches!(
            &active.segments[0],
            StreamSegment::Text(text) if text == "partial answer"
        ));
        assert!(matches!(&active.segments[1], StreamSegment::Tool(_)));
        assert_eq!(app.line_buffer.pending_preview(), "");
    }

    #[test]
    fn completed_turn_prefers_final_content_over_stream_tail() {
        let mut app = test_app();

        app.handle_engine_event(EngineEvent::StreamDelta("first second - item".to_string()));
        app.handle_engine_event(EngineEvent::TurnComplete {
            content: "first\nsecond\n- item".to_string(),
            reasoning: None,
            summary: TurnSummary {
                prompt_tokens: 0,
                completion_tokens: 0,
                cached_tokens: 0,
                elapsed: Duration::from_millis(1),
            },
        });

        let assistant = app.history.iter().find_map(|entry| match entry {
            HistoryEntry::Assistant { content, .. } => Some(content.as_str()),
            _ => None,
        });
        assert_eq!(assistant, Some("first\nsecond\n- item"));
    }

    #[test]
    fn push_text_merges_adjacent() {
        let mut active = ActiveStreaming::default();
        active.push_text("hello ");
        active.push_text("world");
        assert_eq!(active.segments.len(), 1);
        assert!(matches!(&active.segments[0], StreamSegment::Text(s) if s == "hello world"));
    }

    #[test]
    fn agent_state_transitions() {
        let app = test_app();
        assert_eq!(app.agent_state, AgentState::Ready);
        assert!(!app.is_busy());
    }

    #[test]
    fn subagent_tracking() {
        let mut app = test_app();
        app.handle_engine_event(EngineEvent::SubagentStarted {
            task_id: "abc".into(),
            label: "test task".into(),
            task: "do something".into(),
            model: "sub-model".into(),
        });
        assert_eq!(app.subagents.len(), 1);
        assert_eq!(app.running_subagent_count(), 1);
        assert_eq!(app.model, "test");
        assert_eq!(app.subagents.get("abc").unwrap().model, "sub-model");

        app.handle_engine_event(EngineEvent::SubagentCompleted {
            task_id: "abc".into(),
            label: "test task".into(),
            result_preview: "done".into(),
            full_result: "done fully".into(),
        });
        assert_eq!(app.running_subagent_count(), 0);
        assert_eq!(
            app.subagents.get("abc").unwrap().status,
            SubagentStatus::Completed
        );
        assert!(app.subagents.get("abc").unwrap().finished_at.is_some());
    }

    #[test]
    fn clear_command_resets_subagent_render_state() {
        let mut app = test_app();
        app.history.push(HistoryEntry::User("old prompt".into()));
        app.session_msg_count = 3;
        app.pending
            .push_back(QueuedPrompt::user("stale queued".into()));
        app.pending_subagent_results
            .push(("abc".into(), "delegate".into(), "done fully".into()));
        app.held_turn = Some(HeldTurn {
            content: Some("held".into()),
            reasoning: None,
            summary: None,
            note: None,
        });
        app.show_sidebar = true;
        app.handle_engine_event(EngineEvent::SubagentStarted {
            task_id: "abc".into(),
            label: "delegate".into(),
            task: "do something".into(),
            model: "sub-model".into(),
        });

        app.composer.input = "/clear".into();
        app.composer.cursor = app.composer.input.chars().count();
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(app.history.is_empty());
        assert!(app.subagents.is_empty());
        assert!(app.pending_subagent_results.is_empty());
        assert!(app.held_turn.is_none());
        assert!(!app.show_sidebar);
        assert_eq!(app.session_msg_count, 0);
        assert_eq!(app.pending.len(), 1);
        let queued = app.pending.front().unwrap();
        assert_eq!(queued.prompt, "/new");
        assert!(!queued.show_in_history);
    }
}
