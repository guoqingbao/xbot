#![allow(dead_code)]
use std::borrow::Cow;
use std::env;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use console::Term;
use rustyline::completion::Completer;
use rustyline::error::ReadlineError;
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::history::DefaultHistory;
use rustyline::validate::{ValidationContext, ValidationResult, Validator};
use rustyline::{Editor, ExternalPrinter as RustylineExternalPrinter};
use serde_json::Value;
use unicode_width::UnicodeWidthChar;
use unicode_width::UnicodeWidthStr;

use xbot::providers::TextStreamCallback;
use xbot::util::{ensure_dir, tool_emoji, workspace_state_dir};

pub mod channels_cli;
pub mod config_cli;
pub mod skills_cli;

pub use channels_cli::{
    run_channels_list, run_channels_login, run_channels_setup, run_channels_status,
};
pub use config_cli::{run_config_channel, run_config_provider};
pub use skills_cli::{run_skills_init, run_skills_list};
pub enum InputEvent {
    Prompt(String),
    Exit,
    Interrupt,
    Stop,
}

#[derive(Clone)]
struct CliHelper;

impl rustyline::Helper for CliHelper {}

impl Completer for CliHelper {
    type Candidate = String;
}

impl Hinter for CliHelper {
    type Hint = String;
}

impl Validator for CliHelper {
    fn validate(&self, _ctx: &mut ValidationContext) -> rustyline::Result<ValidationResult> {
        Ok(ValidationResult::Valid(None))
    }
}

impl Highlighter for CliHelper {
    fn highlight<'l>(&self, line: &'l str, _pos: usize) -> Cow<'l, str> {
        Cow::Borrowed(line)
    }

    fn highlight_char(
        &self,
        _line: &str,
        _pos: usize,
        _forced: rustyline::highlight::CmdKind,
    ) -> bool {
        false
    }
}

#[derive(Clone)]
struct Style {
    ansi: bool,
}

impl Style {
    fn detect() -> Self {
        Self {
            ansi: io::stdout().is_terminal(),
        }
    }

    fn paint(&self, code: &str, text: impl AsRef<str>) -> String {
        let text = text.as_ref();
        if self.ansi {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }

    fn accent(&self, text: impl AsRef<str>) -> String {
        self.paint("1;36", text)
    }

    fn dim(&self, text: impl AsRef<str>) -> String {
        self.paint("2", text)
    }

    fn error(&self, text: impl AsRef<str>) -> String {
        self.paint("1;31", text)
    }

    fn subtle(&self, text: impl AsRef<str>) -> String {
        self.paint("38;5;245", text)
    }

    fn keyword(&self, text: impl AsRef<str>) -> String {
        self.paint("38;5;75", text)
    }

    fn builtin(&self, text: impl AsRef<str>) -> String {
        self.paint("38;5;141", text)
    }

    fn string(&self, text: impl AsRef<str>) -> String {
        self.paint("38;5;180", text)
    }

    fn number(&self, text: impl AsRef<str>) -> String {
        self.paint("38;5;173", text)
    }

    fn comment(&self, text: impl AsRef<str>) -> String {
        self.paint("38;5;244", text)
    }

    fn bold(&self, text: impl AsRef<str>) -> String {
        self.paint("1", text)
    }

    fn italic(&self, text: impl AsRef<str>) -> String {
        self.paint("3", text)
    }

    fn strike(&self, text: impl AsRef<str>) -> String {
        self.paint("9", text)
    }

    fn inline_code(&self, text: impl AsRef<str>) -> String {
        let text = text.as_ref();
        if self.ansi {
            self.paint("48;5;236;38;5;223", format!(" {text} "))
        } else {
            text.to_string()
        }
    }

    fn link(&self, text: impl AsRef<str>) -> String {
        self.paint("4;38;5;117", text)
    }

    fn code_fence(&self, lang: &str, open: bool) -> String {
        let marker = if open { "┌" } else { "└" };
        let label = if lang.is_empty() { "code" } else { lang };
        if self.ansi {
            self.dim(format!("{marker} {label}"))
        } else {
            format!("{marker} {label}")
        }
    }

    fn panel_line(&self, code: &str, text: impl AsRef<str>, width: usize) -> String {
        let text = pad_to_width(text.as_ref(), width);
        if self.ansi {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text
        }
    }

    fn panel_header(&self, text: impl AsRef<str>, width: usize) -> String {
        self.panel_line("1;38;5;252", text, width)
    }

    fn panel_meta(&self, text: impl AsRef<str>, width: usize) -> String {
        self.panel_line("38;5;245", text, width)
    }

    fn panel_context(&self, text: impl AsRef<str>, width: usize) -> String {
        self.panel_line("38;5;252", text, width)
    }

    fn panel_added(&self, text: impl AsRef<str>, width: usize) -> String {
        self.panel_line("38;5;114", text, width)
    }

    fn panel_removed(&self, text: impl AsRef<str>, width: usize) -> String {
        self.panel_line("38;5;210", text, width)
    }

    fn separator(&self, width: usize) -> String {
        let line = "─".repeat(width.min(60));
        self.dim(line)
    }

    fn queue_pill(&self, text: impl AsRef<str>) -> String {
        let text = text.as_ref();
        if self.ansi {
            self.paint("48;5;153;38;5;23", format!(" ⏳ {text} "))
        } else {
            format!("⏳ {text}")
        }
    }

    fn primary_prompt(&self) -> String {
        if self.ansi {
            format!("{} ", self.accent("›"))
        } else {
            "› ".to_string()
        }
    }

    fn continuation_prompt(&self) -> String {
        if self.ansi {
            format!("{} ", self.subtle("…"))
        } else {
            "…› ".to_string()
        }
    }
}

#[derive(Clone)]
struct OutputTarget {
    style: Style,
    printer: Option<SharedPrinter>,
}

impl OutputTarget {
    fn stdout(style: Style) -> Self {
        Self {
            style,
            printer: None,
        }
    }

    fn printer(style: Style, printer: SharedPrinter) -> Self {
        Self {
            style,
            printer: Some(printer),
        }
    }

    fn write_raw(&self, text: impl AsRef<str>) {
        let text = text.as_ref();
        if let Some(printer) = &self.printer {
            let _ = printer
                .lock()
                .expect("external printer lock poisoned")
                .print(text.to_string());
        } else {
            print!("{text}");
            let _ = io::stdout().flush();
        }
    }

    fn uses_external_printer(&self) -> bool {
        self.printer.is_some()
    }
}

type SharedPrinter = Arc<Mutex<Box<dyn RustylineExternalPrinter + Send>>>;

pub struct TurnSummary {
    pub prompt_tokens: usize,
    pub completion_tokens: usize,
    pub cached_tokens: usize,
    pub elapsed: Duration,
}

impl TurnSummary {
    fn format_one_line(&self, status: &str, style: &Style) -> String {
        let tokens = if self.prompt_tokens == 0 && self.completion_tokens == 0 {
            String::new()
        } else {
            let cache_hint = if self.cached_tokens > 0 && self.prompt_tokens > 0 {
                let pct = (self.cached_tokens * 100) / self.prompt_tokens;
                format!("({}% cached) ", pct)
            } else {
                String::new()
            };
            format!(
                "↑{} {}↓{}",
                self.prompt_tokens, cache_hint, self.completion_tokens
            )
        };
        let elapsed = format_elapsed_short(self.elapsed);
        let mut parts = vec![style.accent(status)];
        if !tokens.is_empty() {
            parts.push(style.subtle(tokens));
        }
        parts.push(style.subtle(format!("last {elapsed}")));
        parts
            .into_iter()
            .filter(|p| !p.is_empty())
            .collect::<Vec<_>>()
            .join(" · ")
    }
}

fn format_elapsed_short(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else {
        let mins = secs / 60;
        let rem = secs % 60;
        format!("{mins}m {rem}s")
    }
}

pub struct CliShell {
    editor: Editor<CliHelper, DefaultHistory>,
    history_path: PathBuf,
    style: Style,
    workspace: PathBuf,
    cwd: PathBuf,
    model: String,
    provider: String,
}

impl CliShell {
    pub fn new(
        workspace: &Path,
        cwd: &Path,
        model: impl Into<String>,
        provider: impl Into<String>,
    ) -> Result<Self> {
        let style = Style::detect();
        let history_path = history_file_path(workspace)?;
        let mut editor = Editor::<CliHelper, DefaultHistory>::new()?;
        editor.set_helper(Some(CliHelper));
        let _ = editor.load_history(&history_path);
        let _ = cwd;
        let model = model.into();
        let provider = provider.into();
        Ok(Self {
            editor,
            history_path,
            style,
            workspace: workspace.to_path_buf(),
            cwd: cwd.to_path_buf(),
            model,
            provider,
        })
    }

    pub fn print_welcome(&self, session_message_count: usize, context_status: &str) {
        let workspace = truncate_middle(&self.workspace.display().to_string(), 72);
        let cwd = truncate_middle(&self.cwd.display().to_string(), 72);
        let state_root = truncate_middle(
            &workspace_state_dir(&self.workspace).display().to_string(),
            72,
        );
        let session_status = if session_message_count == 0 {
            format_session_status(session_message_count)
        } else {
            self.style.bold(format!(
                "continue with {session_message_count} history messages"
            ))
        };
        let primary_commands = if session_message_count == 0 {
            self.style.dim("/help  /clear  /exit  /new")
        } else {
            format!(
                "{}{}",
                self.style.dim("/help  /clear  /exit  "),
                self.style.paint("1;31", "/new (used to start fresh)")
            )
        };
        let rows = vec![
            ("model".to_string(), self.style.accent(&self.model)),
            ("provider".to_string(), self.provider.clone()),
            ("cwd".to_string(), cwd),
            ("workspace".to_string(), workspace),
            ("state".to_string(), state_root),
            ("session".to_string(), session_status),
            ("context".to_string(), context_status.to_string()),
            ("commands".to_string(), primary_commands),
            (
                String::new(),
                self.style
                    .dim("/memorize <text>  /model [name]  /status  /stop"),
            ),
        ];
        println!(
            "{}",
            render_rounded_panel(&self.style, "xbot interactive", &rows)
        );
    }

    pub fn create_output(&mut self) -> Result<CliOutput> {
        let printer = self
            .editor
            .create_external_printer()
            .ok()
            .map(|printer| Arc::new(Mutex::new(Box::new(printer) as Box<_>)));
        let target = printer
            .map(|printer| OutputTarget::printer(self.style.clone(), printer))
            .unwrap_or_else(|| OutputTarget::stdout(self.style.clone()));
        Ok(CliOutput {
            style: self.style.clone(),
            target,
        })
    }

    pub fn read_event(&mut self) -> Result<InputEvent> {
        loop {
            let line = match self.editor.readline(&self.style.primary_prompt()) {
                Ok(line) => line,
                Err(ReadlineError::Interrupted) => return Ok(InputEvent::Interrupt),
                Err(ReadlineError::Eof) => return Ok(InputEvent::Exit),
                Err(err) => return Err(err.into()),
            };

            let input = self.read_multiline(line)?;
            let trimmed = input.trim();
            if trimmed.is_empty() {
                continue;
            }

            match parse_local_command(trimmed) {
                Some(LocalCommand::Exit) => return Ok(InputEvent::Exit),
                Some(LocalCommand::Stop) => return Ok(InputEvent::Stop),
                Some(LocalCommand::Help) => {
                    self.print_help();
                    continue;
                }
                Some(LocalCommand::Clear) => {
                    self.clear_screen();
                    return Ok(InputEvent::Prompt(trimmed.to_string()));
                }
                None => {}
            }

            let _ = self.editor.add_history_entry(trimmed);
            let _ = self.editor.save_history(&self.history_path);
            return Ok(InputEvent::Prompt(trimmed.to_string()));
        }
    }

    pub fn stream_renderer(&self) -> StreamRenderer {
        let target = OutputTarget::stdout(self.style.clone());
        StreamRenderer::new(target)
    }

    fn print_help(&self) {
        let rows = vec![
            (
                "local".to_string(),
                "/help  /clear(screen+reset)  /exit  /stop".to_string(),
            ),
            (
                "agent".to_string(),
                "/new  /memorize <text>  /model [name]  /status".to_string(),
            ),
            (
                "input".to_string(),
                "end a line with \\ for multiline input".to_string(),
            ),
            (
                "queue".to_string(),
                "type while busy to queue the next prompt; /stop or Ctrl-C interrupts".to_string(),
            ),
            (
                "history".to_string(),
                self.history_path.display().to_string(),
            ),
        ];
        println!("{}", render_rounded_panel(&self.style, "CLI Help", &rows));
    }

    fn clear_screen(&self) {
        if self.style.ansi {
            if is_vscode_terminal() {
                // VS Code overlays a sticky terminal header at the top edge.
                // Leave the cursor on the second row so the prompt is not hidden.
                print!("\x1b[2J\x1b[3J\x1b[2;1H");
            } else {
                print!("\x1b[2J\x1b[3J\x1b[H");
            }
            let _ = io::stdout().flush();
        } else {
            println!("\n\n");
        }
    }

    fn read_multiline(&mut self, mut current: String) -> Result<String> {
        while line_requests_continuation(&current) {
            current.pop();
            let next = match self.editor.readline(&self.style.continuation_prompt()) {
                Ok(line) => line,
                Err(ReadlineError::Interrupted) => {
                    println!();
                    continue;
                }
                Err(ReadlineError::Eof) => break,
                Err(err) => return Err(err.into()),
            };
            current.push('\n');
            current.push_str(&next);
        }
        Ok(current)
    }
}

fn is_vscode_terminal() -> bool {
    env::var("TERM_PROGRAM")
        .map(|value| value.eq_ignore_ascii_case("vscode"))
        .unwrap_or(false)
        || env::var_os("VSCODE_IPC_HOOK_CLI").is_some()
}

#[derive(Clone)]
pub struct CliOutput {
    style: Style,
    target: OutputTarget,
}

impl CliOutput {
    pub fn stream_renderer(&self) -> StreamRenderer {
        StreamRenderer::new(self.target.clone())
    }

    pub fn print_queue_notice(&self, queued: usize, prompt: &str) {
        let preview = truncate_middle(prompt.trim(), 64);
        self.target.write_raw(format!(
            "\n{}\n",
            self.style
                .queue_pill(format!("queued #{queued} · {}", preview))
        ));
    }

    pub fn print_dequeue_notice(&self, remaining: usize, prompt: &str) {
        let preview = truncate_middle(prompt.trim(), 64);
        self.target.write_raw(format!(
            "\n{}\n",
            self.style.queue_pill(format!(
                "running queued turn · {}{}",
                preview,
                if remaining > 0 {
                    format!(" · {remaining} still queued")
                } else {
                    String::new()
                }
            ))
        ));
    }

    pub fn print_exit_notice(&self) {
        self.target.write_raw(format!(
            "\n{}\n",
            self.style
                .queue_pill("exit requested · finishing the current turn first")
        ));
    }

    pub fn print_interrupt_notice(&self, queued: usize) {
        self.target.write_raw(format!(
            "\n{}\n\n",
            self.style.error(if queued == 0 {
                "turn cancelled"
            } else {
                "turn cancelled · queued prompt preserved"
            })
        ));
    }
}

#[derive(Clone)]
pub struct StreamRenderer {
    target: OutputTarget,
    state: Arc<Mutex<StreamState>>,
}

struct StreamState {
    started: bool,
    pending: String,
    code_language: Option<String>,
    pending_table: Option<PendingTable>,
    trailing_newlines: usize,
    in_reasoning: bool,
    reasoning_streamed: bool,
    spinner_handle: Option<SpinnerHandle>,
}

impl Default for StreamState {
    fn default() -> Self {
        Self {
            started: false,
            pending: String::new(),
            code_language: None,
            pending_table: None,
            trailing_newlines: 0,
            in_reasoning: false,
            reasoning_streamed: false,
            spinner_handle: None,
        }
    }
}

struct SpinnerHandle {
    stop: Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl SpinnerHandle {
    fn start(target: &OutputTarget) -> Self {
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_clone = stop.clone();
        let style = target.style.clone();
        let thread = std::thread::spawn(move || {
            let frames = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
            let mut i = 0;
            while !stop_clone.load(std::sync::atomic::Ordering::Relaxed) {
                let frame = frames[i % frames.len()];
                let text = style.subtle(format!("  {frame} thinking…"));
                print!("\r{text}\x1b[K");
                let _ = io::stdout().flush();
                i += 1;
                std::thread::sleep(std::time::Duration::from_millis(80));
            }
            print!("\r\x1b[K");
            let _ = io::stdout().flush();
        });
        Self {
            stop,
            thread: Some(thread),
        }
    }

    fn stop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for SpinnerHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

#[derive(Default)]
struct PendingTable {
    lines: Vec<PendingTableLine>,
}

struct PendingTableLine {
    indent: String,
    trimmed: String,
    has_newline: bool,
}

impl StreamRenderer {
    fn new(target: OutputTarget) -> Self {
        Self {
            target,
            state: Arc::new(Mutex::new(StreamState::default())),
        }
    }

    pub fn start_waiting(&self) {
        if !io::stdout().is_terminal() {
            return;
        }
        let mut state = self.state.lock().expect("cli stream state lock poisoned");
        if state.spinner_handle.is_none() {
            state.spinner_handle = Some(SpinnerHandle::start(&self.target));
        }
    }

    pub fn stop_spinner(&self) {
        let mut state = self.state.lock().expect("cli stream state lock poisoned");
        if let Some(mut handle) = state.spinner_handle.take() {
            handle.stop();
        }
    }

    pub fn callback(&self) -> TextStreamCallback {
        let target = self.target.clone();
        let state = self.state.clone();
        Arc::new(Mutex::new(Box::new(move |delta: String| {
            let mut state = state.lock().expect("cli stream state lock poisoned");
            if let Some(mut handle) = state.spinner_handle.take() {
                handle.stop();
            }
            if !state.started {
                target.write_raw("\n".to_string());
                state.started = true;
                note_output(&mut state, "\n");
            }
            if state.in_reasoning {
                state.in_reasoning = false;
                target.write_raw("\n");
                note_output(&mut state, "\n");
                state.trailing_newlines = 1;
            }
            let rendered = render_stream_delta(
                &target.style,
                &mut state,
                &delta,
                false,
                target.uses_external_printer(),
            );
            if !rendered.is_empty() {
                target.write_raw(&rendered);
                note_output(&mut state, &rendered);
            }
        })))
    }

    pub fn reasoning_callback(&self) -> xbot::providers::ReasoningStreamCallback {
        let target = self.target.clone();
        let state = self.state.clone();
        Arc::new(Mutex::new(Box::new(move |delta: String| {
            let mut state = state.lock().expect("cli stream state lock poisoned");
            if let Some(mut handle) = state.spinner_handle.take() {
                handle.stop();
            }
            if !state.started {
                target.write_raw("\n".to_string());
                state.started = true;
                note_output(&mut state, "\n");
            }
            if !state.in_reasoning {
                state.in_reasoning = true;
                state.reasoning_streamed = true;
                target.write_raw(target.style.dim("  Thinking Process: "));
            }
            let styled = target.style.italic(&target.style.dim(&delta));
            target.write_raw(&styled);
        })))
    }

    pub fn tool_hint(&self, hint: &str, tool_name: Option<&str>, tool_args: Option<&Value>) {
        let mut state = self.state.lock().expect("cli stream state lock poisoned");
        if let Some(mut handle) = state.spinner_handle.take() {
            handle.stop();
        }
        if !state.started {
            self.target.write_raw("\n");
            state.started = true;
            note_output(&mut state, "\n");
        }
        let rendered_hint = render_tool_hint(&self.target.style, hint, tool_name, tool_args);
        let mut prefix = String::new();
        if state.trailing_newlines == 0 {
            prefix.push('\n');
        } else if state.trailing_newlines > 1 {
            for _ in 2..=state.trailing_newlines {
                prefix.push_str("\x1b[1A\x1b[2K");
            }
        }
        let rendered = format!("{prefix}{rendered_hint}\n");
        self.target.write_raw(&rendered);
        state.trailing_newlines = rendered_hint
            .chars()
            .rev()
            .take_while(|ch| *ch == '\n')
            .count()
            .max(1);
        drop(state);
    }

    pub fn tool_result(&self, tool_name: &str, success: bool, summary_text: &str) {
        let mut state = self.state.lock().expect("cli stream state lock poisoned");
        if let Some(mut handle) = state.spinner_handle.take() {
            handle.stop();
        }
        if !state.started {
            self.target.write_raw("\n");
            state.started = true;
            note_output(&mut state, "\n");
        }
        let icon = if success { "✓" } else { "✗" };
        let emoji = xbot::util::tool_emoji(tool_name);
        let result_title = format!("{icon} {emoji} {tool_name}");
        let panel_w = available_panel_width();

        let all_lines: Vec<&str> = summary_text.lines().collect();
        let max_preview = 6;
        let preview_lines: Vec<String> = if all_lines.len() <= max_preview {
            all_lines.iter().map(|l| l.to_string()).collect()
        } else {
            let head = 3;
            let tail = 2;
            let mut lines: Vec<String> = all_lines[..head].iter().map(|l| l.to_string()).collect();
            lines.push(format!("  … {} lines …", all_lines.len() - head - tail));
            lines.extend(
                all_lines[all_lines.len() - tail..]
                    .iter()
                    .map(|l| l.to_string()),
            );
            lines
        };

        let max_line_w = panel_w.saturating_sub(8);
        let mut rows: Vec<(String, String)> = Vec::new();
        for (i, line) in preview_lines.iter().enumerate() {
            let plain: String = line.chars().take(max_line_w).collect();
            let styled = if success {
                self.target.style.subtle(&plain)
            } else {
                self.target.style.error(&plain)
            };
            let label = if i == 0 {
                "→".to_string()
            } else {
                String::new()
            };
            rows.push((label, styled));
        }
        if rows.is_empty() {
            rows.push(("→".to_string(), self.target.style.subtle("(empty)")));
        }

        let rendered = render_rounded_panel(&self.target.style, &result_title, &rows);
        let mut prefix = String::new();
        if state.trailing_newlines == 0 {
            prefix.push('\n');
        }
        self.target.write_raw(format!("{prefix}{rendered}\n"));
        state.trailing_newlines = 1;
    }

    pub fn subagent_event(&self, label: &str, event_type: &str) {
        let mut state = self.state.lock().expect("cli stream state lock poisoned");
        if !state.started {
            self.target.write_raw("\n");
            state.started = true;
            note_output(&mut state, "\n");
        }
        let line = match event_type {
            "started" => self
                .target
                .style
                .subtle(format!("  ◐ agent: {label} (started)")),
            "completed" => self
                .target
                .style
                .accent(format!("  ◐ agent: {label} (completed)")),
            "failed" => self
                .target
                .style
                .error(format!("  ◐ agent: {label} (failed)")),
            _ => self
                .target
                .style
                .subtle(format!("  ◐ agent: {label} ({event_type})")),
        };
        self.target.write_raw(format!("{line}\n"));
        state.trailing_newlines = 1;
    }

    pub fn finish(&self, content: &str, reasoning_content: Option<&str>, summary: &TurnSummary) {
        let mut state = self.state.lock().expect("cli stream state lock poisoned");
        if let Some(mut handle) = state.spinner_handle.take() {
            handle.stop();
        }
        if state.in_reasoning {
            state.in_reasoning = false;
            self.target.write_raw("\n");
        }
        let already_streamed_reasoning = state.reasoning_streamed;
        if state.started {
            let tail = render_stream_delta(
                &self.target.style,
                &mut state,
                "",
                true,
                self.target.uses_external_printer(),
            );
            if !tail.is_empty() {
                self.target.write_raw(&tail);
                note_output(&mut state, &tail);
            }
            if state.trailing_newlines == 0 {
                self.target.write_raw("\n");
            }
            if !already_streamed_reasoning {
                if let Some(reasoning) = reasoning_content {
                    if !reasoning.trim().is_empty() {
                        let rendered_reasoning = self.render_reasoning_content(reasoning);
                        self.target.write_raw(&rendered_reasoning);
                    }
                }
            }
            self.target
                .write_raw(format!("\n{}\n", self.target.style.separator(60)));
            state.trailing_newlines = 1;
        } else {
            if !already_streamed_reasoning {
                if let Some(reasoning) = reasoning_content {
                    let rendered_reasoning = self.render_reasoning_content(reasoning);
                    self.target.write_raw(rendered_reasoning);
                    note_output(&mut state, "\n");
                }
            }
            let rendered = render_markdown_response(&self.target.style, content);
            self.target.write_raw(format!(
                "\n{}\n\n{}\n",
                rendered,
                self.target.style.separator(60)
            ));
            note_output(&mut state, &rendered);
        }
        // Print one-line status after turn completes
        let status_line = summary.format_one_line("ready", &self.target.style);
        self.target.write_raw(format!("\n{}\n", status_line));
    }

    fn render_reasoning_content(&self, reasoning: &str) -> String {
        let lines: Vec<&str> = reasoning.lines().collect();
        if lines.is_empty() {
            return String::new();
        }
        let mut out = String::new();
        out.push_str(&self.target.style.dim("\n  Thinking Process: "));
        for (i, line) in lines.iter().enumerate() {
            if i == 0 {
                out.push_str(
                    &self
                        .target
                        .style
                        .italic(&self.target.style.dim(format!("{line}\n"))),
                );
            } else {
                out.push_str(
                    &self
                        .target
                        .style
                        .italic(&self.target.style.dim(format!("  {line}\n"))),
                );
            }
        }
        out
    }

    pub fn finish_empty(&self, note: &str, summary: &TurnSummary) {
        let mut state = self.state.lock().expect("cli stream state lock poisoned");
        if let Some(mut handle) = state.spinner_handle.take() {
            handle.stop();
        }
        let tail = render_stream_delta(
            &self.target.style,
            &mut state,
            "",
            true,
            self.target.uses_external_printer(),
        );
        if !tail.is_empty() {
            self.target.write_raw(&tail);
            note_output(&mut state, &tail);
        }
        if !note.trim().is_empty() {
            let rendered = format!("\n{}\n", self.target.style.subtle(format!("· {note}")));
            self.target.write_raw(&rendered);
            note_output(&mut state, &rendered);
        }
        // Print one-line status after turn completes
        let status_line = summary.format_one_line("ready", &self.target.style);
        self.target.write_raw(format!("\n{}\n", status_line));
    }

    pub fn finish_error(&self, err: &str) {
        let mut state = self.state.lock().expect("cli stream state lock poisoned");
        if let Some(mut handle) = state.spinner_handle.take() {
            handle.stop();
        }
        let tail = render_stream_delta(
            &self.target.style,
            &mut state,
            "",
            true,
            self.target.uses_external_printer(),
        );
        if !tail.is_empty() {
            self.target.write_raw(&tail);
            note_output(&mut state, &tail);
        }
        let rendered = format!("\n{} {}\n", self.target.style.error("error:"), err);
        self.target.write_raw(&rendered);
        note_output(&mut state, &rendered);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LocalCommand {
    Help,
    Clear,
    Exit,
    Stop,
}

fn parse_local_command(input: &str) -> Option<LocalCommand> {
    let trimmed = input.trim().to_lowercase();
    match trimmed.as_str() {
        "/help" | "help" => Some(LocalCommand::Help),
        "/clear" | "clear" => Some(LocalCommand::Clear),
        "/stop" | "stop" | "[stop]" => Some(LocalCommand::Stop),
        "/exit" | "/quit" | "exit" | "quit" => Some(LocalCommand::Exit),
        _ => None,
    }
}

fn line_requests_continuation(input: &str) -> bool {
    input.ends_with('\\') && !input.ends_with("\\\\")
}

fn history_file_path(workspace: &Path) -> Result<PathBuf> {
    let root = workspace_state_dir(workspace);
    ensure_dir(&root)?;
    Ok(root.join("history.txt"))
}

fn parse_tool_hint(hint: &str) -> ToolHintParts<'_> {
    let (tool_name, detail) = hint.split_once(" · ").unwrap_or((hint, ""));
    ToolHintParts {
        emoji: tool_emoji(tool_name),
        tool_name,
        detail,
    }
}

fn parse_legacy_tool_hint(hint: &str) -> Option<(&str, &str)> {
    let trimmed = hint
        .strip_prefix("[ ")
        .and_then(|value| value.strip_suffix(" ]"))?;
    let without_emoji = trimmed.split_once(' ')?.1.trim_start();
    let (tool_name, detail) = without_emoji
        .split_once("  ")
        .unwrap_or((without_emoji, ""));
    Some((tool_name.trim(), detail.trim()))
}

fn wrap_panel_text(text: &str, max_width: usize) -> Vec<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }

    let mut lines = Vec::new();
    let mut current = String::new();
    for word in trimmed.split_whitespace() {
        let candidate_width = if current.is_empty() {
            char_width(word)
        } else {
            char_width(&current) + 1 + char_width(word)
        };
        if !current.is_empty() && candidate_width > max_width {
            lines.push(current);
            current = word.to_string();
        } else {
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(word);
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(trimmed.to_string());
    }
    lines
}

fn summarize_tool_hint_args(args: &Value) -> String {
    match args {
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
            for key in preferred_keys {
                let Some(value) = map.get(key) else {
                    continue;
                };
                let summary = summarize_tool_hint_value(value);
                if !summary.is_empty() {
                    parts.push(format!("{key}={summary}"));
                }
            }
            if parts.is_empty() {
                map.iter()
                    .take(2)
                    .filter_map(|(key, value)| {
                        let summary = summarize_tool_hint_value(value);
                        (!summary.is_empty()).then(|| format!("{key}={summary}"))
                    })
                    .collect::<Vec<_>>()
                    .join(" · ")
            } else {
                parts.join(" · ")
            }
        }
        _ => summarize_tool_hint_value(args),
    }
}

fn summarize_tool_hint_value(value: &Value) -> String {
    match value {
        Value::String(text) => truncate_middle(text.trim(), 48),
        Value::Array(values) => {
            let items = values
                .iter()
                .take(3)
                .map(summarize_tool_hint_value)
                .filter(|item| !item.is_empty())
                .collect::<Vec<_>>();
            if values.len() > 3 {
                format!("{} …", items.join(", "))
            } else {
                items.join(", ")
            }
        }
        Value::Number(number) => number.to_string(),
        Value::Bool(flag) => flag.to_string(),
        Value::Null => String::new(),
        Value::Object(_) => "{...}".to_string(),
    }
}

fn terminal_columns() -> usize {
    let (_, columns) = Term::stdout().size();
    usize::from(columns).max(1)
}

fn available_panel_width() -> usize {
    terminal_columns().saturating_sub(4).max(1)
}

fn render_rounded_panel(style: &Style, title: &str, rows: &[(String, String)]) -> String {
    let label_width = rows
        .iter()
        .map(|(label, _)| label.chars().count())
        .max()
        .unwrap_or(0);
    let rendered_rows = rows
        .iter()
        .map(|(label, value)| {
            if label.is_empty() {
                let indent = if label_width == 0 {
                    String::new()
                } else {
                    " ".repeat(label_width + 2)
                };
                format!("{indent}{value}")
            } else {
                format!("{label:label_width$}  {value}")
            }
        })
        .collect::<Vec<_>>();
    let natural_width = rendered_rows
        .iter()
        .map(|row| char_width(row))
        .max()
        .unwrap_or(0)
        .max(char_width(title) + 1);
    let avail = available_panel_width();
    let content_width = if natural_width > avail / 2 {
        avail
    } else {
        natural_width.max(30)
    };
    let top_fill = content_width.saturating_sub(char_width(title) + 1);

    let mut out = vec![format!(
        "{}{}{}",
        style.dim("╭─ "),
        style.accent(title),
        style.dim(format!(" {}╮", "─".repeat(top_fill)))
    )];
    if rendered_rows.is_empty() {
        out.push(format!(
            "{}{}{}",
            style.dim("╰"),
            style.dim("─".repeat(content_width + 2)),
            style.dim("╯")
        ));
        return out.join("\n");
    }

    for row in rendered_rows {
        out.push(format!(
            "{} {} {}",
            style.dim("│"),
            pad_to_width(&row, content_width),
            style.dim("│")
        ));
    }
    out.push(format!(
        "{}{}{}",
        style.dim("╰"),
        style.dim("─".repeat(content_width + 2)),
        style.dim("╯")
    ));
    out.join("\n")
}

fn render_rounded_block(style: &Style, title: &str, lines: &[String], width: usize) -> String {
    let content_width = width.max(char_width(title) + 1);
    let top_fill = content_width.saturating_sub(char_width(title) + 1);

    let mut out = vec![format!(
        "{}{}{}",
        style.dim("╭─ "),
        style.accent(title),
        style.dim(format!(" {}╮", "─".repeat(top_fill)))
    )];
    if lines.is_empty() {
        out.push(format!(
            "{}{}{}",
            style.dim("╰"),
            style.dim("─".repeat(content_width + 2)),
            style.dim("╯")
        ));
        return out.join("\n");
    }

    for line in lines {
        out.push(format!(
            "{} {} {}",
            style.dim("│"),
            pad_to_width(line, content_width),
            style.dim("│")
        ));
    }
    out.push(format!(
        "{}{}{}",
        style.dim("╰"),
        style.dim("─".repeat(content_width + 2)),
        style.dim("╯")
    ));
    out.join("\n")
}

fn render_status_panel(style: &Style, content: &str) -> Option<String> {
    let mut lines = content.lines();
    let version = lines.next()?.strip_prefix("xbot v")?;
    let model = lines.next()?.strip_prefix("Model: ")?;
    let tokens = lines.next()?.strip_prefix("Tokens: ")?;
    let context = lines.next()?.strip_prefix("Context: ")?;
    let session = lines.next()?.strip_prefix("Session: ")?;
    let uptime = lines.next()?.strip_prefix("Uptime: ")?;
    if lines.next().is_some() {
        return None;
    }

    let rows = vec![
        ("version".to_string(), format!("v{version}")),
        ("model".to_string(), style.accent(model)),
        ("tokens".to_string(), tokens.to_string()),
        ("context".to_string(), context.to_string()),
        ("session".to_string(), session.to_string()),
        ("uptime".to_string(), uptime.to_string()),
    ];
    Some(render_rounded_panel(style, "status", &rows))
}

fn render_model_panel(style: &Style, content: &str) -> Option<String> {
    if let Some(switched) = content.strip_prefix("Model switched to ") {
        let (model, context) =
            if let Some((name, suffix)) = switched.split_once(" (context window ") {
                (
                    name.trim(),
                    Some(suffix.trim_end_matches(')').trim().to_string()),
                )
            } else {
                (switched.trim(), None)
            };
        let mut rows = vec![("active".to_string(), style.accent(model))];
        if let Some(context) = context {
            rows.push(("context".to_string(), context));
        }
        return Some(render_rounded_panel(style, "model", &rows));
    }

    let mut lines = content.lines();
    let current_model = lines.next()?.strip_prefix("Current model: ")?;
    let mut rows = vec![("current".to_string(), style.accent(current_model))];

    let mut next_line = lines.next()?;
    if let Some(context) = next_line.strip_prefix("Context window: ") {
        rows.push(("context".to_string(), context.to_string()));
        next_line = lines.next()?;
    }
    if next_line != "Available models:" {
        return None;
    }

    let models = lines
        .map(|line| line.strip_prefix("- ").map(str::trim))
        .collect::<Option<Vec<_>>>()?;
    if models.is_empty() {
        rows.push(("available".to_string(), "none".to_string()));
    } else {
        rows.push(("available".to_string(), models[0].to_string()));
        for model in models.iter().skip(1) {
            rows.push((String::new(), (*model).to_string()));
        }
    }
    Some(render_rounded_panel(style, "model", &rows))
}

fn render_tool_hint(
    style: &Style,
    hint: &str,
    tool_name: Option<&str>,
    tool_args: Option<&Value>,
) -> String {
    if tool_name == Some("edit_file") {
        if let Some(rendered) = render_edit_file_hint(style, hint, tool_args) {
            return rendered;
        }
    }
    let (resolved_tool_name, detail) = if let Some(tool_name) = tool_name {
        (
            tool_name,
            tool_args
                .map(summarize_tool_hint_args)
                .filter(|detail| !detail.is_empty())
                .unwrap_or_else(|| {
                    parse_legacy_tool_hint(hint)
                        .map(|(_, detail)| detail.to_string())
                        .unwrap_or_else(|| parse_tool_hint(hint).detail.to_string())
                }),
        )
    } else if let Some((tool_name, detail)) = parse_legacy_tool_hint(hint) {
        (tool_name, detail.to_string())
    } else {
        let parts = parse_tool_hint(hint);
        (parts.tool_name, parts.detail.to_string())
    };

    let title = format!("{} {resolved_tool_name}", tool_emoji(resolved_tool_name));
    let wrapped_detail = wrap_panel_text(&detail, 76);
    let mut rows = if wrapped_detail.is_empty() {
        vec![("state".to_string(), style.subtle("running"))]
    } else {
        wrapped_detail
            .into_iter()
            .enumerate()
            .map(|(index, line)| {
                if index == 0 {
                    ("detail".to_string(), style.subtle(line))
                } else {
                    (String::new(), style.subtle(line))
                }
            })
            .collect::<Vec<_>>()
    };
    if rows.is_empty() {
        rows.push(("state".to_string(), style.subtle("running")));
    }
    render_rounded_panel(style, &title, &rows)
}

fn render_edit_file_hint(style: &Style, _hint: &str, tool_args: Option<&Value>) -> Option<String> {
    let args = tool_args?.as_object()?;
    let path = args.get("path")?.as_str()?;
    let old_text = args
        .get("old_text")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .replace("\r\n", "\n");
    let new_text = args
        .get("new_text")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .replace("\r\n", "\n");
    let width = available_panel_width();
    let computed = xbot::diff::compute_diff(&old_text, &new_text);
    let mut lines = Vec::new();
    lines.push(style.panel_meta(
        format!("path    {}", truncate_middle(path, width.saturating_sub(8))),
        width,
    ));
    lines.push(style.panel_header(
        format!(" {:>5} {:>5} |   diff preview", "old", "new"),
        width,
    ));
    lines.extend(render_diff_lines(style, &computed.lines, width));
    Some(render_rounded_block(style, "✍ edit_file", &lines, width))
}

fn render_diff_lines(style: &Style, lines: &[xbot::diff::DiffLine], width: usize) -> Vec<String> {
    use xbot::diff::DiffKind;
    lines
        .iter()
        .map(|line| {
            let old = line
                .old_lineno
                .map(|value| format!("{value:>5}"))
                .unwrap_or_else(|| "     ".to_string());
            let new = line
                .new_lineno
                .map(|value| format!("{value:>5}"))
                .unwrap_or_else(|| "     ".to_string());
            let prefix = format!(" {old} {new} | {} ", line.marker);
            let text_width = width.saturating_sub(char_width(&prefix));
            let content = truncate_end(&line.text, text_width);
            let formatted = format!("{prefix}{content}");
            match line.kind {
                DiffKind::Context => style.panel_context(formatted, width),
                DiffKind::Added => style.panel_added(formatted, width),
                DiffKind::Removed => style.panel_removed(formatted, width),
                DiffKind::Omitted => style.panel_meta(formatted, width),
            }
        })
        .collect()
}

#[allow(dead_code)]
struct ToolHintParts<'a> {
    emoji: &'static str,
    tool_name: &'a str,
    detail: &'a str,
}

fn render_markdown_response(style: &Style, content: &str) -> String {
    if let Some(panel) = render_status_panel(style, content) {
        return panel;
    }
    if let Some(panel) = render_model_panel(style, content) {
        return panel;
    }
    let mut state = StreamState::default();
    render_stream_delta(style, &mut state, content, true, false)
}

fn render_stream_delta(
    style: &Style,
    state: &mut StreamState,
    delta: &str,
    flush_all: bool,
    line_safe_mode: bool,
) -> String {
    state.pending.push_str(delta);
    let mut out = String::new();
    loop {
        if let Some(pos) = state.pending.find('\n') {
            let line = state.pending[..=pos].to_string();
            state.pending.replace_range(..=pos, "");
            out.push_str(&render_stream_line(style, state, &line));
            continue;
        }
        if flush_all {
            if !state.pending.is_empty() {
                let line = std::mem::take(&mut state.pending);
                out.push_str(&render_stream_line(style, state, &line));
            }
            if state.code_language.is_none() && state.pending_table.is_some() {
                out.push_str(&flush_pending_table(style, state));
            }
            break;
        }
        if state.code_language.is_none() {
            if line_safe_mode {
                if let Some(index) = sentence_flush_index(&state.pending) {
                    let chunk = state.pending[..index].to_string();
                    state.pending.replace_range(..index, "");
                    out.push_str(&chunk);
                    continue;
                }
            } else if !state.pending.is_empty() {
                out.push_str(&state.pending);
                state.pending.clear();
            }
        }
        break;
    }
    out
}

fn sentence_flush_index(text: &str) -> Option<usize> {
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for (idx, ch) in text.char_indices() {
        if let Some(active) = quote {
            if escaped {
                escaped = false;
                continue;
            }
            if ch == '\\' {
                escaped = true;
                continue;
            }
            if ch == active {
                quote = None;
            }
            continue;
        }
        if ch == '"' || ch == '\'' || ch == '`' {
            quote = Some(ch);
            continue;
        }
        if matches!(ch, '.' | '!' | '?') {
            // Don't break at numbered list markers like "1. " or "10. "
            if ch == '.' {
                let before = &text[..idx];
                if before.ends_with(|c: char| c.is_ascii_digit()) {
                    continue;
                }
            }
            let next = text[idx + ch.len_utf8()..].chars().next();
            if next.is_none_or(char::is_whitespace) {
                return Some(idx + ch.len_utf8());
            }
        }
    }
    None
}

fn note_output(state: &mut StreamState, text: &str) {
    if text.is_empty() {
        return;
    }
    let trailing = text
        .chars()
        .rev()
        .filter(|ch| *ch != '\r')
        .take_while(|ch| *ch == '\n')
        .count();
    if trailing > 0 {
        if text.chars().all(|ch| ch == '\n' || ch == '\r') {
            state.trailing_newlines += trailing;
        } else {
            state.trailing_newlines = trailing;
        }
    } else {
        state.trailing_newlines = 0;
    }
}

fn render_stream_line(style: &Style, state: &mut StreamState, line: &str) -> String {
    let has_newline = line.ends_with('\n');
    let normalized = line.trim_end_matches('\n').trim_end_matches('\r');
    if state.code_language.is_none() {
        if let Some((indent, trimmed)) = parse_table_line(normalized) {
            state
                .pending_table
                .get_or_insert_with(PendingTable::default)
                .lines
                .push(PendingTableLine {
                    indent: indent.to_string(),
                    trimmed: trimmed.to_string(),
                    has_newline,
                });
            return String::new();
        }
        if state.pending_table.is_some() {
            let mut flushed = flush_pending_table(style, state);
            if !normalized.is_empty() || has_newline {
                flushed.push_str(&render_stream_line_without_tables(
                    style,
                    state,
                    normalized,
                    has_newline,
                ));
            }
            return flushed;
        }
    }
    render_stream_line_without_tables(style, state, normalized, has_newline)
}

fn render_stream_line_without_tables(
    style: &Style,
    state: &mut StreamState,
    normalized: &str,
    has_newline: bool,
) -> String {
    if let Some(lang) = fence_language(normalized) {
        if state.code_language.is_none() {
            state.code_language = Some(lang.to_string());
            return format!("\n{}\n", style.code_fence(lang, true));
        }
        let previous = state.code_language.take().unwrap_or_default();
        return format!("{}\n", style.code_fence(&previous, false));
    }
    if let Some(lang) = state.code_language.as_deref() {
        let highlighted = highlight_code_line(style, normalized, lang);
        if has_newline {
            format!("{highlighted}\n")
        } else {
            highlighted
        }
    } else {
        let rendered = render_markdown_line(style, normalized);
        if has_newline {
            format!("{rendered}\n")
        } else {
            rendered
        }
    }
}

fn flush_pending_table(style: &Style, state: &mut StreamState) -> String {
    let Some(table) = state.pending_table.take() else {
        return String::new();
    };
    render_table_block(style, table)
}

fn render_markdown_line(style: &Style, line: &str) -> String {
    if line.is_empty() {
        return line.to_string();
    }

    let indent_len = line.len() - line.trim_start().len();
    let indent = &line[..indent_len];
    let trimmed = line.trim_start();

    if let Some(content) = parse_heading(trimmed) {
        return format!(
            "{indent}{}",
            style.accent(render_inline_markdown(style, content))
        );
    }

    if is_horizontal_rule(trimmed) {
        return format!("{indent}{}", style.dim("────────────────────────────────"));
    }

    if let Some((depth, content)) = parse_blockquote(trimmed) {
        let prefix = style.dim("│ ".repeat(depth.max(1)));
        return format!(
            "{indent}{prefix}{}",
            style.dim(render_inline_markdown(style, content))
        );
    }

    if let Some((checked, content)) = parse_task_item(trimmed) {
        let marker = if checked { "[x]" } else { "[ ]" };
        return format!(
            "{indent}{} {}",
            style.accent(marker),
            render_inline_markdown(style, content)
        );
    }

    if let Some((number, content)) = parse_ordered_item(trimmed) {
        return format!(
            "{indent}{} {}",
            style.accent(format!("{number}.")),
            render_inline_markdown(style, content)
        );
    }

    if let Some(content) = parse_unordered_item(trimmed) {
        return format!(
            "{indent}{} {}",
            style.accent("•"),
            render_inline_markdown(style, content)
        );
    }

    if trimmed.starts_with('|') && trimmed.ends_with('|') {
        let cells = trimmed
            .trim_matches('|')
            .split('|')
            .map(|cell| render_inline_markdown(style, cell.trim()))
            .collect::<Vec<_>>()
            .join(&style.dim(" │ "));
        return format!("{indent}{cells}");
    }

    render_inline_markdown(style, line)
}

fn parse_table_line(line: &str) -> Option<(&str, &str)> {
    let indent_len = line.len() - line.trim_start().len();
    let indent = &line[..indent_len];
    let trimmed = line.trim_start();
    (trimmed.starts_with('|') && trimmed.ends_with('|')).then_some((indent, trimmed))
}

fn parse_table_cells(line: &str) -> Vec<&str> {
    line.trim_matches('|').split('|').map(str::trim).collect()
}

fn is_table_separator_row(line: &str) -> bool {
    let cells = parse_table_cells(line);
    !cells.is_empty()
        && cells.iter().all(|cell| {
            !cell.is_empty()
                && cell
                    .chars()
                    .all(|ch| ch == '-' || ch == ':' || ch.is_ascii_whitespace())
                && cell.contains('-')
        })
}

fn render_table_block(style: &Style, table: PendingTable) -> String {
    if table.lines.is_empty() {
        return String::new();
    }

    if table.lines.len() >= 2 && is_table_separator_row(&table.lines[1].trimmed) {
        render_aligned_table(style, &table.lines)
    } else {
        table
            .lines
            .iter()
            .map(|line| {
                let rendered =
                    render_markdown_line(style, &format!("{}{}", line.indent, line.trimmed));
                if line.has_newline {
                    format!("{rendered}\n")
                } else {
                    rendered
                }
            })
            .collect::<String>()
    }
}

fn render_aligned_table(style: &Style, lines: &[PendingTableLine]) -> String {
    let indent = lines
        .first()
        .map(|line| line.indent.as_str())
        .unwrap_or_default();
    render_aligned_table_with_width(style, lines, available_table_width(indent))
}

fn render_aligned_table_with_width(
    style: &Style,
    lines: &[PendingTableLine],
    max_table_width: usize,
) -> String {
    let indent = lines
        .first()
        .map(|line| line.indent.as_str())
        .unwrap_or_default();
    let mut rows = lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| {
            (!is_table_separator_row(&line.trimmed) || index != 1).then_some(
                parse_table_cells(&line.trimmed)
                    .into_iter()
                    .map(|cell| render_inline_markdown(style, cell))
                    .collect::<Vec<_>>(),
            )
        })
        .collect::<Vec<_>>();
    let column_count = rows.iter().map(Vec::len).max().unwrap_or(0);
    for row in &mut rows {
        row.resize(column_count, String::new());
    }
    let mut widths = (0..column_count)
        .map(|column| {
            rows.iter()
                .map(|row| char_width(&row[column]))
                .max()
                .unwrap_or(0)
        })
        .collect::<Vec<_>>();
    constrain_column_widths(&mut widths, max_table_width);

    let separator = widths
        .iter()
        .map(|width| style.dim("─".repeat(*width)))
        .collect::<Vec<_>>()
        .join(&style.dim("─┼─"));

    let mut rendered_lines = rows
        .into_iter()
        .enumerate()
        .flat_map(|(index, row)| {
            let rendered_row = row
                .iter()
                .enumerate()
                .map(|(column, cell)| fit_table_cell(cell, widths[column]))
                .collect::<Vec<_>>()
                .join(&style.dim(" │ "));
            if index == 0 {
                vec![
                    format!("{indent}{rendered_row}"),
                    format!("{indent}{separator}"),
                ]
            } else {
                vec![format!("{indent}{rendered_row}")]
            }
        })
        .collect::<Vec<_>>()
        .join("\n");

    if lines.last().is_some_and(|line| line.has_newline) {
        rendered_lines.push('\n');
    }
    rendered_lines
}

fn available_table_width(indent: &str) -> usize {
    let (_, columns) = Term::stdout().size();
    usize::from(columns)
        .saturating_sub(char_width(indent))
        .saturating_sub(1)
}

fn constrain_column_widths(widths: &mut [usize], max_table_width: usize) {
    if widths.is_empty() {
        return;
    }

    let separator_width = (widths.len() - 1) * 3;
    let mut total_width = widths.iter().sum::<usize>() + separator_width;
    if total_width <= max_table_width {
        return;
    }

    let mut min_widths = widths
        .iter()
        .map(|width| (*width).min(3))
        .collect::<Vec<_>>();
    let min_total = min_widths.iter().sum::<usize>() + separator_width;
    if min_total > max_table_width {
        min_widths.fill(1);
    }

    while total_width > max_table_width {
        let Some((index, _)) = widths
            .iter()
            .enumerate()
            .filter(|(index, width)| **width > min_widths[*index])
            .max_by_key(|(_, width)| **width)
        else {
            break;
        };
        widths[index] -= 1;
        total_width -= 1;
    }
}

fn fit_table_cell(text: &str, width: usize) -> String {
    let visible_width = char_width(text);
    if visible_width <= width {
        return pad_to_width(text, width);
    }

    let plain = strip_ansi_escapes(text);
    pad_to_width(&truncate_end(&plain, width), width)
}

fn render_inline_markdown(style: &Style, text: &str) -> String {
    let mut out = String::new();
    let mut index = 0usize;
    while index < text.len() {
        let remaining = &text[index..];
        if let Some(rest) = remaining.strip_prefix('\\') {
            if let Some(ch) = rest.chars().next() {
                out.push(ch);
                index += 1 + ch.len_utf8();
            } else {
                index += 1;
            }
            continue;
        }
        if let Some((consumed, rendered)) = parse_link(style, remaining) {
            out.push_str(&rendered);
            index += consumed;
            continue;
        }
        if let Some((consumed, rendered)) =
            parse_delimited_span(style, remaining, "`", |s, inner| s.inline_code(inner))
        {
            out.push_str(&rendered);
            index += consumed;
            continue;
        }
        if let Some((consumed, rendered)) =
            parse_delimited_span(style, remaining, "**", |s, inner| s.bold(inner))
        {
            out.push_str(&rendered);
            index += consumed;
            continue;
        }
        if let Some((consumed, rendered)) =
            parse_delimited_span(style, remaining, "__", |s, inner| s.bold(inner))
        {
            out.push_str(&rendered);
            index += consumed;
            continue;
        }
        if let Some((consumed, rendered)) =
            parse_delimited_span(style, remaining, "~~", |s, inner| s.strike(inner))
        {
            out.push_str(&rendered);
            index += consumed;
            continue;
        }
        if let Some((consumed, rendered)) =
            parse_delimited_span(style, remaining, "*", |s, inner| s.italic(inner))
        {
            out.push_str(&rendered);
            index += consumed;
            continue;
        }
        if let Some((consumed, rendered)) =
            parse_delimited_span(style, remaining, "_", |s, inner| s.italic(inner))
        {
            out.push_str(&rendered);
            index += consumed;
            continue;
        }

        let ch = remaining.chars().next().expect("non-empty remaining");
        out.push(ch);
        index += ch.len_utf8();
    }
    out
}

fn parse_heading(line: &str) -> Option<&str> {
    let level = line.chars().take_while(|ch| *ch == '#').count();
    (1..=6)
        .contains(&level)
        .then_some(&line[level..])
        .and_then(|rest| rest.strip_prefix(' '))
}

fn is_horizontal_rule(line: &str) -> bool {
    let stripped = line.replace([' ', '\t'], "");
    stripped.len() >= 3
        && (stripped.chars().all(|ch| ch == '-')
            || stripped.chars().all(|ch| ch == '*')
            || stripped.chars().all(|ch| ch == '_'))
}

fn parse_blockquote(line: &str) -> Option<(usize, &str)> {
    let mut rest = line;
    let mut depth = 0usize;
    while let Some(stripped) = rest.strip_prefix('>') {
        depth += 1;
        rest = stripped.strip_prefix(' ').unwrap_or(stripped);
    }
    (depth > 0).then_some((depth, rest.trim_start()))
}

fn parse_task_item(line: &str) -> Option<(bool, &str)> {
    let rest = line
        .strip_prefix("- ")
        .or_else(|| line.strip_prefix("* "))
        .or_else(|| line.strip_prefix("+ "))?;
    if let Some(content) = rest.strip_prefix("[ ] ") {
        return Some((false, content));
    }
    if let Some(content) = rest
        .strip_prefix("[x] ")
        .or_else(|| rest.strip_prefix("[X] "))
    {
        return Some((true, content));
    }
    None
}

fn parse_ordered_item(line: &str) -> Option<(usize, &str)> {
    let digits = line.chars().take_while(|ch| ch.is_ascii_digit()).count();
    if digits == 0 || !line[digits..].starts_with(". ") {
        return None;
    }
    let number = line[..digits].parse().ok()?;
    Some((number, &line[digits + 2..]))
}

fn parse_unordered_item(line: &str) -> Option<&str> {
    line.strip_prefix("- ")
        .or_else(|| line.strip_prefix("* "))
        .or_else(|| line.strip_prefix("+ "))
}

fn parse_link(style: &Style, text: &str) -> Option<(usize, String)> {
    let closing_label = text.find("](")?;
    let label = text.strip_prefix('[')?;
    let label = &label[..closing_label - 1];
    let url_start = closing_label + 2;
    let closing_url = text[url_start..].find(')')?;
    let url = &text[url_start..url_start + closing_url];
    let consumed = url_start + closing_url + 1;
    Some((
        consumed,
        format!(
            "{}{}",
            style.link(label),
            style.subtle(format!(" ({})", truncate_middle(url, 48)))
        ),
    ))
}

fn parse_delimited_span<F>(
    style: &Style,
    text: &str,
    delimiter: &str,
    render: F,
) -> Option<(usize, String)>
where
    F: Fn(&Style, &str) -> String,
{
    let inner = text.strip_prefix(delimiter)?;
    let end = find_closing_delimiter(inner, delimiter)?;
    let content = &inner[..end];
    Some((delimiter.len() * 2 + end, render(style, content)))
}

fn find_closing_delimiter(text: &str, delimiter: &str) -> Option<usize> {
    let mut escaped = false;
    let mut index = 0usize;
    while index < text.len() {
        let remaining = &text[index..];
        let ch = remaining.chars().next()?;
        if escaped {
            escaped = false;
            index += ch.len_utf8();
            continue;
        }
        if ch == '\\' {
            escaped = true;
            index += 1;
            continue;
        }
        if remaining.starts_with(delimiter) {
            return Some(index);
        }
        index += ch.len_utf8();
    }
    None
}

fn fence_language(line: &str) -> Option<&str> {
    let trimmed = line.trim();
    trimmed.strip_prefix("```").map(|lang| lang.trim())
}

fn highlight_code_line(style: &Style, line: &str, language: &str) -> String {
    if !style.ansi || line.is_empty() {
        return line.to_string();
    }
    let language = normalize_language(language);
    let (code, comment) = split_comment(line, language);
    let mut out = highlight_code_tokens(style, code, language);
    if let Some(comment) = comment {
        out.push_str(&style.comment(comment));
    }
    out
}

fn normalize_language(language: &str) -> &str {
    match language.to_ascii_lowercase().as_str() {
        "rs" => "rust",
        "py" => "python",
        "js" => "javascript",
        "ts" => "typescript",
        "tsx" => "tsx",
        "jsx" => "jsx",
        "sh" => "bash",
        "zsh" => "bash",
        "yml" => "yaml",
        "md" => "markdown",
        "jsonc" => "json",
        other => Box::leak(other.to_string().into_boxed_str()),
    }
}

fn split_comment<'a>(line: &'a str, language: &str) -> (&'a str, Option<&'a str>) {
    let delimiter = match language {
        "python" | "bash" | "yaml" | "dockerfile" | "ruby" => Some("#"),
        "sql" => Some("--"),
        "rust" | "javascript" | "typescript" | "jsx" | "tsx" | "java" | "go" | "c" | "cpp"
        | "swift" | "kotlin" | "csharp" => Some("//"),
        _ => None,
    };
    let Some(delimiter) = delimiter else {
        return (line, None);
    };
    let Some(index) = find_comment_start(line, delimiter) else {
        return (line, None);
    };
    (&line[..index], Some(&line[index..]))
}

fn find_comment_start(line: &str, delimiter: &str) -> Option<usize> {
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for (idx, ch) in line.char_indices() {
        if let Some(active) = quote {
            if escaped {
                escaped = false;
                continue;
            }
            if ch == '\\' {
                escaped = true;
                continue;
            }
            if ch == active {
                quote = None;
            }
            continue;
        }
        if ch == '"' || ch == '\'' || ch == '`' {
            quote = Some(ch);
            continue;
        }
        if line[idx..].starts_with(delimiter) {
            return Some(idx);
        }
    }
    None
}

fn highlight_code_tokens(style: &Style, code: &str, language: &str) -> String {
    let keywords = language_keywords(language);
    let builtins = builtin_keywords(language);
    let mut out = String::new();
    let chars = code.chars().collect::<Vec<_>>();
    let mut index = 0usize;
    while index < chars.len() {
        let ch = chars[index];
        if ch == '"' || ch == '\'' || ch == '`' {
            let start = index;
            index += 1;
            let mut escaped = false;
            while index < chars.len() {
                let current = chars[index];
                if escaped {
                    escaped = false;
                } else if current == '\\' {
                    escaped = true;
                } else if current == ch {
                    index += 1;
                    break;
                }
                index += 1;
            }
            out.push_str(&style.string(chars[start..index].iter().collect::<String>()));
            continue;
        }
        if is_number_start(&chars, index) {
            let start = index;
            index += 1;
            while index < chars.len()
                && (chars[index].is_ascii_hexdigit()
                    || matches!(chars[index], '_' | '.' | 'x' | 'X' | 'o' | 'O' | 'b' | 'B'))
            {
                index += 1;
            }
            out.push_str(&style.number(chars[start..index].iter().collect::<String>()));
            continue;
        }
        if is_identifier_start(ch) {
            let start = index;
            index += 1;
            while index < chars.len() && is_identifier_continue(chars[index]) {
                index += 1;
            }
            let token = chars[start..index].iter().collect::<String>();
            if keywords.contains(&token.as_str()) {
                out.push_str(&style.keyword(token));
            } else if builtins.contains(&token.as_str()) {
                out.push_str(&style.builtin(token));
            } else {
                out.push_str(&token);
            }
            continue;
        }
        out.push(ch);
        index += 1;
    }
    out
}

fn language_keywords(language: &str) -> &'static [&'static str] {
    match language {
        "rust" => &[
            "fn", "let", "mut", "pub", "impl", "struct", "enum", "trait", "async", "await",
            "match", "if", "else", "for", "while", "loop", "return", "use", "mod", "const",
            "static", "where", "Self", "self",
        ],
        "python" => &[
            "def", "class", "async", "await", "if", "elif", "else", "for", "while", "return",
            "import", "from", "try", "except", "finally", "with", "as", "pass", "yield",
        ],
        "javascript" | "typescript" | "jsx" | "tsx" => &[
            "function", "const", "let", "var", "class", "extends", "async", "await", "if", "else",
            "for", "while", "return", "import", "from", "export", "new", "switch", "case",
            "default", "try", "catch",
        ],
        "bash" => &[
            "if", "then", "else", "fi", "for", "do", "done", "case", "esac", "function", "in",
        ],
        "json" => &[],
        _ => &[
            "if", "else", "for", "while", "return", "class", "function", "const", "let", "var",
        ],
    }
}

fn builtin_keywords(language: &str) -> &'static [&'static str] {
    match language {
        "rust" => &["Some", "None", "Ok", "Err", "true", "false"],
        "python" => &["True", "False", "None"],
        "javascript" | "typescript" | "jsx" | "tsx" => &["true", "false", "null", "undefined"],
        "json" => &["true", "false", "null"],
        _ => &["true", "false", "null"],
    }
}

fn is_identifier_start(ch: char) -> bool {
    ch == '_' || ch.is_ascii_alphabetic()
}

fn is_identifier_continue(ch: char) -> bool {
    ch == '_' || ch.is_ascii_alphanumeric()
}

fn is_number_start(chars: &[char], index: usize) -> bool {
    let ch = chars[index];
    if !ch.is_ascii_digit() {
        return false;
    }
    if index > 0 && is_identifier_continue(chars[index - 1]) {
        return false;
    }
    true
}

fn char_width(text: &str) -> usize {
    UnicodeWidthStr::width(strip_ansi_escapes(text).as_str())
}

fn truncate_display_width(text: &str, max_width: usize) -> String {
    let plain = strip_ansi_escapes(text);
    let visible_width = UnicodeWidthStr::width(plain.as_str());
    if visible_width <= max_width {
        return plain;
    }
    if max_width == 0 {
        return String::new();
    }
    if max_width == 1 {
        return "…".to_string();
    }

    let mut out = String::new();
    let mut width = 0usize;
    for ch in plain.chars() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + ch_width > max_width.saturating_sub(1) {
            break;
        }
        out.push(ch);
        width += ch_width;
    }
    out.push('…');
    out
}

fn strip_ansi_escapes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_escape = false;
    for ch in text.chars() {
        if in_escape {
            if ch == 'm' {
                in_escape = false;
            }
            continue;
        }
        if ch == '\x1b' {
            in_escape = true;
            continue;
        }
        out.push(ch);
    }
    out
}

fn pad_to_width(text: &str, width: usize) -> String {
    let visible_width = char_width(text);
    if visible_width == width {
        return text.to_string();
    }
    if visible_width > width {
        return truncate_display_width(text, width);
    }
    let mut out = String::with_capacity(text.len() + width.saturating_sub(visible_width));
    out.push_str(text);
    out.push_str(&" ".repeat(width - visible_width));
    out
}

fn truncate_end(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_string();
    }
    if max_chars <= 1 {
        return "…".to_string();
    }
    let mut out = text.chars().take(max_chars - 1).collect::<String>();
    out.push('…');
    out
}

fn truncate_middle(text: &str, max_chars: usize) -> String {
    let chars = text.chars().count();
    if chars <= max_chars {
        return text.to_string();
    }
    let head = max_chars / 2 - 2;
    let tail = max_chars.saturating_sub(head + 3);
    let start = text.chars().take(head).collect::<String>();
    let end = text
        .chars()
        .rev()
        .take(tail)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();
    format!("{start}...{end}")
}

fn format_session_status(session_message_count: usize) -> String {
    if session_message_count == 0 {
        "new session".to_string()
    } else {
        let label = if session_message_count == 1 {
            "message"
        } else {
            "messages"
        };
        format!("resuming {session_message_count} {label}; /new to start fresh")
    }
}

#[cfg(test)]
mod tests {
    use super::{
        LocalCommand, PendingTableLine, StreamState, available_panel_width, char_width,
        fence_language, format_session_status, highlight_code_line, line_requests_continuation,
        parse_local_command, parse_tool_hint, render_aligned_table_with_width,
        render_markdown_line, render_markdown_response, render_stream_delta, render_tool_hint,
        sentence_flush_index, truncate_middle,
    };
    use serde_json::json;

    #[test]
    fn parses_local_commands() {
        assert_eq!(parse_local_command("/help"), Some(LocalCommand::Help));
        assert_eq!(parse_local_command("/clear"), Some(LocalCommand::Clear));
        assert_eq!(parse_local_command("/stop"), Some(LocalCommand::Stop));
        assert_eq!(parse_local_command("quit"), Some(LocalCommand::Exit));
        assert_eq!(parse_local_command("/status"), None);
        assert_eq!(parse_local_command("/model"), None);
    }

    #[test]
    fn detects_multiline_continuation() {
        assert!(line_requests_continuation("hello \\"));
        assert!(!line_requests_continuation("hello"));
        assert!(!line_requests_continuation("hello \\\\"));
    }

    #[test]
    fn truncates_middle_for_long_paths() {
        let value = truncate_middle("/very/long/path/to/a/project/workspace/directory", 20);
        assert!(value.contains("..."));
        assert!(value.len() <= 20);
    }

    #[test]
    fn formats_session_status_for_new_and_existing_sessions() {
        assert_eq!(format_session_status(0), "new session");
        assert_eq!(
            format_session_status(1),
            "resuming 1 message; /new to start fresh"
        );
        assert_eq!(
            format_session_status(11),
            "resuming 11 messages; /new to start fresh"
        );
    }

    #[test]
    fn parses_tool_hint_and_emoji() {
        let parts = parse_tool_hint("read_file · path=src/main.rs");
        assert_eq!(parts.emoji, "📖");
        assert_eq!(parts.tool_name, "read_file");
        assert_eq!(parts.detail, "path=src/main.rs");
    }

    #[test]
    fn detects_fence_language() {
        assert_eq!(fence_language("```rust"), Some("rust"));
        assert_eq!(fence_language("```"), Some(""));
        assert_eq!(fence_language("fn main() {}"), None);
    }

    #[test]
    fn highlights_rust_keywords_when_ansi_enabled() {
        let style = super::Style { ansi: true };
        let rendered = highlight_code_line(&style, "let value = Some(42);", "rust");
        assert!(rendered.contains("\u{1b}[38;5;75mlet\u{1b}[0m"));
        assert!(rendered.contains("\u{1b}[38;5;141mSome\u{1b}[0m"));
    }

    #[test]
    fn sentence_flush_finds_boundary() {
        assert_eq!(sentence_flush_index("Hello world. Next"), Some(12));
        assert_eq!(sentence_flush_index("path=src/main.rs"), None);
        assert_eq!(sentence_flush_index("### Phase 1: Critical"), None);
        assert_eq!(sentence_flush_index("2026-03-25T08:"), None);
    }

    #[test]
    fn renders_markdown_blocks_without_markup_noise() {
        let style = super::Style { ansi: false };
        assert_eq!(render_markdown_line(&style, "# Heading"), "Heading");
        assert_eq!(render_markdown_line(&style, "- item"), "• item");
        assert_eq!(render_markdown_line(&style, "> note"), "│ note");
        assert_eq!(render_markdown_line(&style, "`code`"), "code");
    }

    #[test]
    fn renders_markdown_tables_without_literal_separator_rows() {
        let style = super::Style { ansi: false };
        let content = "## Bug Severity Distribution\n\n| Severity | Count | Percentage |\n|----------|-------|------------|\n| High | 4 | 25% |\n| Medium | 10 | 62.5% |\n| Low | 2 | 12.5% |\n| **Total** | **16** | **100%** |";
        let rendered = render_markdown_response(&style, content);
        assert!(rendered.contains("Severity │ Count │ Percentage"));
        assert!(rendered.contains("┼"));
        assert!(!rendered.contains("---------- │ ------- │ ------------"));
        assert!(rendered.contains("Total    │ 16    │ 100%"));
    }

    #[test]
    fn renders_status_response_as_closed_panel() {
        let style = super::Style { ansi: false };
        let content = "xbot v0.1.2\nModel: qwen\nTokens: 10 in / 22 out\nContext: 512/4096 (12%)\nSession: 8 history messages\nUptime: 2m 4s";
        let rendered = render_markdown_response(&style, content);
        assert!(rendered.contains("╭─ status"));
        assert!(rendered.contains("│ version"));
        assert!(rendered.contains("│ model"));
        assert!(rendered.contains("╰"));
        assert!(rendered.contains("╯"));
    }

    #[test]
    fn expands_status_panel_to_terminal_width() {
        let style = super::Style { ansi: false };
        let content = "xbot v0.1.2\nModel: qwen\nTokens: 10 in / 22 out\nContext: 512/4096 (12%)\nSession: 8 history messages\nUptime: 2m 4s";
        let rendered = render_markdown_response(&style, content);
        let widths: Vec<usize> = rendered.lines().map(char_width).collect();
        let panel_width = *widths.iter().max().unwrap_or(&0);
        assert!(panel_width >= 30, "panel should be at least 30 wide");
        assert!(
            panel_width <= available_panel_width() + 4,
            "panel should not exceed terminal width"
        );
        for line in rendered.lines() {
            assert_eq!(char_width(line), panel_width, "{line}");
        }
    }

    #[test]
    fn renders_model_response_as_closed_panel() {
        let style = super::Style { ansi: false };
        let content =
            "Current model: qwen\nContext window: 131072\nAvailable models:\n- qwen\n- llama";
        let rendered = render_markdown_response(&style, content);
        assert!(rendered.contains("╭─ model"));
        assert!(rendered.contains("│ current"));
        assert!(rendered.contains("│ available"));
        assert!(rendered.contains("llama"));
        assert!(rendered.contains("╯"));
    }

    #[test]
    fn streams_markdown_tables_as_aligned_blocks() {
        let style = super::Style { ansi: false };
        let mut state = StreamState::default();
        let mut rendered = String::new();
        rendered.push_str(&render_stream_delta(
            &style,
            &mut state,
            "| Severity | Count | Percentage |\n",
            false,
            false,
        ));
        rendered.push_str(&render_stream_delta(
            &style,
            &mut state,
            "|----------|-------|------------|\n",
            false,
            false,
        ));
        rendered.push_str(&render_stream_delta(
            &style,
            &mut state,
            "| High | 4 | 25% |\n",
            false,
            false,
        ));
        rendered.push_str(&render_stream_delta(
            &style,
            &mut state,
            "| Medium | 10 | 62.5% |\n",
            false,
            false,
        ));
        rendered.push_str(&render_stream_delta(&style, &mut state, "", true, false));
        assert!(rendered.contains("Severity │ Count │ Percentage"));
        assert!(rendered.contains("High     │ 4     │ 25%"));
        assert!(rendered.contains("Medium   │ 10    │ 62.5%"));
        assert!(!rendered.contains("---------- │ ------- │ ------------"));
    }

    #[test]
    fn constrains_wide_tables_to_available_width() {
        let style = super::Style { ansi: false };
        let lines = vec![
            PendingTableLine {
                indent: String::new(),
                trimmed: "| Metric | Value |".to_string(),
                has_newline: true,
            },
            PendingTableLine {
                indent: String::new(),
                trimmed: "|--------|-------|".to_string(),
                has_newline: true,
            },
            PendingTableLine {
                indent: String::new(),
                trimmed: "| Channels Affected | 4 (Telegram, Slack, Feishu, Email) |".to_string(),
                has_newline: true,
            },
            PendingTableLine {
                indent: String::new(),
                trimmed: "| Files Requiring Changes | 6 |".to_string(),
                has_newline: false,
            },
        ];

        let rendered = render_aligned_table_with_width(&style, &lines, 36);
        for line in rendered.lines() {
            assert!(char_width(line) <= 36, "{line}");
        }
        assert!(rendered.contains("Channels"));
        assert!(rendered.contains("Files"));
        assert!(rendered.contains('…'));
    }

    #[test]
    fn constrains_multi_column_tables_without_wrapping() {
        let style = super::Style { ansi: false };
        let lines = vec![
            PendingTableLine {
                indent: String::new(),
                trimmed: "| # | Channel | Bug Description | Risk | Fix Complexity |".to_string(),
                has_newline: true,
            },
            PendingTableLine {
                indent: String::new(),
                trimmed: "|---|---------|-----------------|------|----------------|".to_string(),
                has_newline: true,
            },
            PendingTableLine {
                indent: String::new(),
                trimmed: "| 3 | Telegram | Path traversal - external filenames not sanitized | Security | Low |".to_string(),
                has_newline: false,
            },
        ];

        let rendered = render_aligned_table_with_width(&style, &lines, 60);
        for line in rendered.lines() {
            assert!(char_width(line) <= 60, "{line}");
        }
        assert!(rendered.contains("Path traversal"));
        assert!(rendered.contains('…'));
    }

    #[test]
    fn line_safe_streaming_keeps_colon_chunks_buffered() {
        let style = super::Style { ansi: false };
        let mut state = StreamState::default();
        assert_eq!(
            render_stream_delta(&style, &mut state, "### Phase 1:", false, true),
            ""
        );
        assert_eq!(
            render_stream_delta(
                &style,
                &mut state,
                " Critical (Do Immediately)",
                false,
                true
            ),
            ""
        );
        assert_eq!(
            render_stream_delta(
                &style,
                &mut state,
                " **Report Generated:** 2026-03-25T08:",
                false,
                true
            ),
            ""
        );
        assert_eq!(
            render_stream_delta(&style, &mut state, "13:", false, true),
            ""
        );
        let rendered = render_stream_delta(&style, &mut state, "00Z", true, true);
        assert!(rendered.contains("Phase 1: Critical (Do Immediately)"));
        assert!(rendered.contains("Report Generated: 2026-03-25T08:13:00Z"));
    }

    #[test]
    fn renders_edit_file_hint_as_diff_panel() {
        let style = super::Style { ansi: false };
        let args = json!({
            "path": "/root/xbot/src/cli/mod.rs",
            "old_text": "a\nb\nc\n",
            "new_text": "a\nbeta\nc\nd\n",
            "replace_all": false
        });
        let rendered = render_tool_hint(
            &style,
            "edit_file · path=/root/xbot/src/cli/mod.rs · old_text=a... · new_text=beta...",
            Some("edit_file"),
            Some(&args),
        );
        assert!(rendered.contains("edit_file"));
        assert!(rendered.contains("mod.rs"));
        assert!(rendered.contains("beta"));
        assert!(rendered.contains("old   new |   diff preview"));
    }

    #[test]
    fn edit_file_hint_rows_stay_aligned_and_full_width() {
        let style = super::Style { ansi: false };
        let args = json!({
            "path": "/root/xbot/src/tools.rs",
            "old_text": "\"number\" => match value {\n    Value::String(text) => text.parse::<f64>().map(Value::from).unwrap_or_else(|_| value.clone()),\n    _ => value.clone(),\n},\n",
            "new_text": "\"number\" => match value {\n    Value::String(text) => match text.parse::<f64>() {\n        Ok(num) => Value::from(num),\n        Err(_) => Value::String(format!(\"[invalid number: {}]\", text)),\n    },\n    _ => value.clone(),\n},\n",
            "replace_all": false
        });
        let rendered = render_tool_hint(
            &style,
            "edit_file · path=/root/xbot/src/tools.rs",
            Some("edit_file"),
            Some(&args),
        );
        let expected_width = available_panel_width() + 4;
        for line in rendered.lines() {
            assert_eq!(char_width(line), expected_width, "{line}");
        }
        assert!(rendered.contains(" old   new |   diff preview"));
    }

    #[test]
    fn renders_tool_hint_as_closed_panel() {
        let style = super::Style { ansi: false };
        let args = json!({"path": "src/main.rs"});
        let rendered = render_tool_hint(
            &style,
            "[ 📖 read_file  path=src/main.rs ]",
            Some("read_file"),
            Some(&args),
        );
        assert!(rendered.contains("╭─ 📖 read_file"));
        assert!(rendered.contains("│ detail"));
        assert!(rendered.contains("path=src/main.rs"));
        assert!(!rendered.contains("[ 📖"));
        assert!(rendered.contains("╯"));
    }

    #[test]
    fn tool_hint_panel_with_emoji_title_matches_terminal_width() {
        let style = super::Style { ansi: false };
        let args = json!({"path": "/root/xbot/src/channels/telegram.rs"});
        let rendered = render_tool_hint(
            &style,
            "read_file · path=/root/xbot/src/channels/telegram.rs",
            Some("read_file"),
            Some(&args),
        );
        let widths: Vec<usize> = rendered.lines().map(char_width).collect();
        let panel_width = *widths.iter().max().unwrap_or(&0);
        assert!(panel_width >= 30, "panel should be at least 30 wide");
        assert!(
            panel_width <= available_panel_width() + 4,
            "panel should not exceed terminal width"
        );
        for line in rendered.lines() {
            assert_eq!(char_width(line), panel_width, "{line}");
        }
    }
}
