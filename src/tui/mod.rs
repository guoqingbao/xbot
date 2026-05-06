mod app;
mod markdown;
mod ui;

pub use app::{EngineEvent, TurnSummary};

use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use crossterm::{cursor, execute};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::mpsc;

use app::{AgentState as AS, App};
use rbot::engine::AgentLoop;
use rbot::engine::subtasks::SubagentNotification;
use rbot::providers::TextStreamCallback;
use rbot::storage::OutboundMessage;
use rbot::tools::MessageSendCallback;

const ACTIVE_POLL_MS: u64 = 16;
const IDLE_POLL_MS: u64 = 40;

struct FrameRateLimiter {
    last_emitted: Option<Instant>,
}

impl Default for FrameRateLimiter {
    fn default() -> Self {
        Self { last_emitted: None }
    }
}

impl FrameRateLimiter {
    fn time_until_next_draw(&self, now: Instant) -> Option<Duration> {
        let last = self.last_emitted?;
        let interval = Duration::from_nanos(8_333_334); // ~120 FPS
        let next = last + interval;
        if next <= now { None } else { Some(next - now) }
    }

    fn mark_emitted(&mut self, now: Instant) {
        self.last_emitted = Some(now);
    }
}

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let mut stdout = io::stdout();
        let _ = execute!(
            stdout,
            DisableMouseCapture,
            DisableBracketedPaste,
            LeaveAlternateScreen,
            cursor::Show
        );
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn run_tui_repl(
    agent: Arc<AgentLoop>,
    model: String,
    provider_name: String,
    workspace: PathBuf,
    _cwd: PathBuf,
    session_key: String,
    chat_id: String,
    session_msg_count: usize,
    context_status: String,
    subagent_model: Option<String>,
) -> Result<()> {
    install_panic_hook();

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste,
        cursor::Hide
    )?;
    let _guard = TerminalGuard;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let mut app = App::new(
        model,
        provider_name,
        workspace,
        session_msg_count,
        context_status,
        subagent_model,
    );

    let (engine_tx, mut engine_rx) = mpsc::unbounded_channel::<EngineEvent>();

    let subagent_tx = engine_tx.clone();
    agent.set_subagent_notification_callback(Some(Arc::new(move |notif| {
        let event = match notif {
            SubagentNotification::Started {
                task_id,
                label,
                task,
                model,
            } => EngineEvent::SubagentStarted {
                task_id,
                label,
                task,
                model,
            },
            SubagentNotification::Progress {
                task_id,
                tool_name,
                detail,
                step,
            } => EngineEvent::SubagentProgress {
                task_id,
                tool_name,
                detail,
                step,
            },
            SubagentNotification::Completed {
                task_id,
                label,
                result_preview,
                full_result,
            } => EngineEvent::SubagentCompleted {
                task_id,
                label,
                result_preview,
                full_result,
            },
            SubagentNotification::Failed {
                task_id,
                label,
                error,
            } => EngineEvent::SubagentFailed {
                task_id,
                label,
                error,
            },
            SubagentNotification::Cancelled { task_id } => {
                EngineEvent::SubagentCancelled { task_id }
            }
        };
        let _ = subagent_tx.send(event);
    })));

    let (input_tx, mut input_rx) = mpsc::unbounded_channel::<crossterm::event::Event>();
    std::thread::spawn(move || {
        loop {
            match crossterm::event::poll(Duration::from_millis(50)) {
                Ok(true) => {
                    if let Ok(event) = crossterm::event::read() {
                        if input_tx.send(event).is_err() {
                            break;
                        }
                    }
                }
                Ok(false) => {}
                Err(_) => break,
            }
        }
    });

    let mut active_turn: Option<tokio::task::JoinHandle<()>> = None;
    let mut frame_limiter = FrameRateLimiter::default();

    loop {
        while let Ok(event) = engine_rx.try_recv() {
            app.handle_engine_event(event);
        }

        while let Ok(event) = input_rx.try_recv() {
            app.handle_crossterm_event(event);
        }

        if app.cancel_requested {
            app.cancel_requested = false;
            if let Some(handle) = active_turn.take() {
                handle.abort();
                agent.set_progress_sender(None);
            }
            app.agent_state = AS::Ready;
            app.pending.clear();
            app.flush_active_as_cancelled();
            app.needs_redraw = true;
        }

        if !app.is_busy() {
            if let Some(prompt) = app.pending.pop_front() {
                active_turn = Some(spawn_turn(
                    agent.clone(),
                    prompt,
                    session_key.clone(),
                    chat_id.clone(),
                    engine_tx.clone(),
                ));
                app.agent_state = AS::Working;
                app.needs_redraw = true;
            }
        }

        app.tick_animation();

        let now = Instant::now();
        if app.needs_redraw {
            if frame_limiter.time_until_next_draw(now).is_none() {
                terminal.draw(|f| ui::render(f, &mut app))?;
                frame_limiter.mark_emitted(Instant::now());
                app.needs_redraw = false;
            }
        }

        if app.should_quit {
            break;
        }

        let poll_ms = if app.is_busy() || app.running_subagent_count() > 0 {
            ACTIVE_POLL_MS
        } else {
            IDLE_POLL_MS
        };
        let mut sleep_dur = Duration::from_millis(poll_ms);
        if let Some(draw_wait) = frame_limiter.time_until_next_draw(Instant::now()) {
            sleep_dur = sleep_dur.min(draw_wait);
        }
        sleep_dur = sleep_dur.max(Duration::from_millis(1));
        tokio::time::sleep(sleep_dur).await;
    }

    agent.set_subagent_notification_callback(None);
    drop(_guard);
    Ok(())
}

fn spawn_turn(
    agent: Arc<AgentLoop>,
    prompt: String,
    session_key: String,
    chat_id: String,
    tx: mpsc::UnboundedSender<EngineEvent>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let stream_callback = make_stream_callback(tx.clone());
        agent.set_progress_sender(Some(make_progress_callback(
            tx.clone(),
            agent.clone(),
            session_key.clone(),
        )));
        let started = Instant::now();

        let result = agent
            .process_direct_stream(
                &prompt,
                &session_key,
                "cli",
                &chat_id,
                Some(stream_callback),
            )
            .await;

        let summary_from_snapshot = |elapsed| {
            agent
                .snapshot()
                .map(|snap| TurnSummary {
                    prompt_tokens: snap.last_prompt_tokens,
                    completion_tokens: snap.last_completion_tokens,
                    cached_tokens: snap.last_cached_tokens,
                    elapsed,
                })
                .unwrap_or(TurnSummary {
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    cached_tokens: 0,
                    elapsed,
                })
        };

        match result {
            Ok(Some(response)) => {
                let summary = summary_from_snapshot(started.elapsed());
                let _ = tx.send(EngineEvent::TurnComplete {
                    content: response.content,
                    reasoning: response.reasoning_content,
                    summary,
                });
            }
            Ok(None) => {
                let summary = summary_from_snapshot(started.elapsed());
                let _ = tx.send(EngineEvent::TurnEmpty {
                    note: "no direct reply".to_string(),
                    summary,
                });
            }
            Err(err) => {
                let _ = tx.send(EngineEvent::TurnError(format!("{err:#}")));
            }
        }

        agent.set_progress_sender(None);

        if let Ok(status_content) = agent.session_status_content(&session_key).await {
            if let Some(ctx) = status_content
                .lines()
                .find_map(|line| line.strip_prefix("Context: ").map(ToOwned::to_owned))
            {
                let _ = tx.send(EngineEvent::ContextUpdate(ctx));
            }
        }
    })
}

fn make_stream_callback(tx: mpsc::UnboundedSender<EngineEvent>) -> TextStreamCallback {
    Arc::new(std::sync::Mutex::new(Box::new(move |delta: String| {
        let _ = tx.send(EngineEvent::StreamDelta(delta));
    })))
}

fn make_progress_callback(
    tx: mpsc::UnboundedSender<EngineEvent>,
    agent: Arc<AgentLoop>,
    session_key: String,
) -> MessageSendCallback {
    Arc::new(move |msg: OutboundMessage| {
        let tx = tx.clone();
        let agent = agent.clone();
        let session_key = session_key.clone();
        Box::pin(async move {
            if msg
                .metadata
                .get("_context_update")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
            {
                if let Some(ctx) = msg
                    .metadata
                    .get("_context")
                    .and_then(serde_json::Value::as_str)
                    .map(String::from)
                {
                    let _ = tx.send(EngineEvent::ContextUpdate(ctx));
                }
                return Ok(());
            }

            if msg
                .metadata
                .get("_tool_hint")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
            {
                let tool_name = msg
                    .metadata
                    .get("_tool_name")
                    .and_then(serde_json::Value::as_str)
                    .map(String::from);

                let is_summarizing = msg
                    .metadata
                    .get("_tool_args")
                    .and_then(|v| v.get("_summarizing"))
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);

                let is_summarizing_done = msg
                    .metadata
                    .get("_tool_args")
                    .and_then(|v| v.get("_summarizing_done"))
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);

                if is_summarizing {
                    let _ = tx.send(EngineEvent::Summarizing);
                } else if is_summarizing_done {
                    let _ = tx.send(EngineEvent::SummarizingDone);
                } else {
                    let _ = tx.send(EngineEvent::ToolHint {
                        tool_name,
                        tool_args: msg.metadata.get("_tool_args").cloned(),
                    });
                }

                if let Ok(status_content) = agent.session_status_content(&session_key).await {
                    if let Some(ctx) = status_content
                        .lines()
                        .find_map(|line| line.strip_prefix("Context: ").map(ToOwned::to_owned))
                    {
                        let _ = tx.send(EngineEvent::ContextUpdate(ctx));
                    }
                }
            }
            Ok(())
        })
    })
}

fn install_panic_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let mut stdout = io::stdout();
        let _ = execute!(
            stdout,
            DisableMouseCapture,
            DisableBracketedPaste,
            LeaveAlternateScreen,
            cursor::Show
        );
        original(info);
    }));
}
