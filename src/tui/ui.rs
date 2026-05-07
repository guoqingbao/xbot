use std::collections::BTreeSet;

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap,
};

use super::app::{
    ActiveStreaming, AgentState, App, EditDiff, HistoryEntry, StreamSegment, SubagentStatus,
};
use super::markdown;

const HEADER_BG: Color = Color::Rgb(20, 24, 36);
const BORDER_DIM: Color = Color::Rgb(55, 65, 85);
const TEXT_PRIMARY: Color = Color::White;
const TEXT_MUTED: Color = Color::Rgb(120, 130, 150);
const TEXT_DIM: Color = Color::Rgb(80, 90, 105);
const ACCENT: Color = Color::Rgb(80, 180, 230);
const USER_FG: Color = Color::Rgb(230, 195, 80);
const ERROR_FG: Color = Color::Rgb(230, 80, 80);
const SUCCESS_FG: Color = Color::Rgb(80, 200, 120);
const TOOL_FG: Color = Color::Rgb(130, 170, 210);
const DIFF_ADD: Color = Color::Rgb(80, 200, 120);
const DIFF_DEL: Color = Color::Rgb(220, 90, 90);
const DIFF_HEADER: Color = Color::Rgb(100, 160, 220);
const WORKING_FG: Color = Color::Rgb(230, 195, 80);
const SUMMARIZE_FG: Color = Color::Rgb(180, 140, 230);
const SEPARATOR_FG: Color = Color::Rgb(55, 65, 85);
const COMPOSER_BG: Color = Color::Rgb(16, 20, 30);
const TRANSCRIPT_BG: Color = Color::Rgb(12, 14, 22);
const SIDEBAR_BG: Color = Color::Rgb(14, 17, 26);
const SUBAGENT_RUNNING: Color = Color::Rgb(100, 180, 230);
const SUBAGENT_DONE: Color = Color::Rgb(80, 200, 120);
const SUBAGENT_FAIL: Color = Color::Rgb(230, 80, 80);

const SIDEBAR_MIN_WIDTH: u16 = 100;
const SIDEBAR_WIDTH: u16 = 28;

pub fn render(f: &mut Frame, app: &mut App) {
    let area = f.area();

    let composer_h = composer_height(&app.composer.input, area.width);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(composer_h),
            Constraint::Length(1),
        ])
        .split(area);

    render_header(f, chunks[0], app);

    let body = chunks[1];
    if app.show_sidebar && body.width >= SIDEBAR_MIN_WIDTH {
        let sidebar_w = SIDEBAR_WIDTH.min(body.width.saturating_sub(40));
        let split = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(1), Constraint::Length(sidebar_w)])
            .split(body);
        render_transcript(f, split[0], app);
        render_sidebar(f, split[1], app);
    } else {
        render_transcript(f, body, app);
    }

    render_composer(f, chunks[2], app);
    render_footer(f, chunks[3], app);

    if app.show_help {
        render_help_overlay(f, area);
    }

    if app.approval_dialog.is_some() {
        render_approval_overlay(f, area, app);
    }
}

fn composer_height(input: &str, area_width: u16) -> u16 {
    let inner_w = area_width.saturating_sub(2).max(1) as usize;
    let mut visual_lines: usize = 0;
    for line in input.split('\n') {
        let chars = line.chars().count();
        if inner_w > 0 && chars > inner_w {
            visual_lines += (chars + inner_w - 1) / inner_w;
        } else {
            visual_lines += 1;
        }
    }
    (visual_lines as u16 + 2).min(12).max(3)
}

fn render_header(f: &mut Frame, area: Rect, app: &App) {
    let (status_icon, status_color) = match app.agent_state {
        AgentState::Working => (format!("{} working", app.spinner_char()), WORKING_FG),
        AgentState::WaitingSubagents => (
            format!("{} waiting agents", app.spinner_char()),
            SUBAGENT_RUNNING,
        ),
        AgentState::Summarizing => (format!("{} summarizing", app.spinner_char()), SUMMARIZE_FG),
        AgentState::Ready => ("● ready".to_string(), SUCCESS_FG),
    };

    let sep = Span::styled(" │ ", Style::default().fg(BORDER_DIM));

    let mut spans = vec![
        Span::styled(
            " ■ rbot ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        sep.clone(),
        Span::styled(
            format!("main {}", app.model),
            Style::default().fg(TEXT_PRIMARY),
        ),
        sep.clone(),
        Span::styled(app.provider.clone(), Style::default().fg(TEXT_MUTED)),
        sep.clone(),
        Span::styled(
            format!("ctx:{}", app.context_status),
            Style::default().fg(TEXT_MUTED),
        ),
        sep.clone(),
        Span::styled(status_icon, Style::default().fg(status_color)),
    ];

    let subagent_models = distinct_subagent_models(app);
    if !subagent_models.is_empty() {
        spans.push(sep.clone());
        spans.push(Span::styled(
            format!("agents {}", subagent_models.join(", ")),
            Style::default().fg(SUBAGENT_RUNNING),
        ));
    }

    let running = app.running_subagent_count();
    if running > 0 {
        spans.push(Span::styled(
            format!(
                " │ ◐ {running} agent{}",
                if running == 1 { "" } else { "s" }
            ),
            Style::default().fg(SUBAGENT_RUNNING),
        ));
    }

    if !app.pending.is_empty() {
        spans.push(Span::styled(
            format!(" +{} queued", app.pending.len()),
            Style::default().fg(TEXT_DIM),
        ));
    }

    let header = Paragraph::new(Line::from(spans)).style(Style::default().bg(HEADER_BG));
    f.render_widget(header, area);
}

fn render_transcript(f: &mut Frame, area: Rect, app: &mut App) {
    let inner_width = area.width.saturating_sub(2) as usize;
    let inner_height = area.height.saturating_sub(2) as usize;

    let mut lines = build_transcript_lines(app, inner_width);

    if let Some(ref active) = app.active {
        lines.extend(build_active_lines(
            active,
            app,
            inner_width,
            app.animation_frame,
        ));
    }

    let pending_preview = app.line_buffer.pending_preview();
    if !pending_preview.is_empty() && app.is_busy() {
        let md = markdown::markdown_to_lines(pending_preview, inner_width.saturating_sub(2).max(1));
        for ml in md {
            let mut prefixed = vec![Span::raw("  ")];
            prefixed.extend(ml.spans);
            lines.push(Line::from(prefixed));
        }
    }

    if app.agent_state == AgentState::WaitingSubagents {
        lines.push(Line::from(""));
        let running = app.running_subagent_count();
        let total = app.subagents.len();
        let done = total.saturating_sub(running);
        let header_text = format!(
            "  {} Waiting for subagents ({done}/{total} done)…",
            app.spinner_char()
        );
        lines.push(Line::from(Span::styled(
            header_text,
            Style::default()
                .fg(SUBAGENT_RUNNING)
                .add_modifier(Modifier::BOLD),
        )));
        for (label, status) in app.waiting_subagent_lines() {
            let (icon, color) = match status {
                SubagentStatus::Running => {
                    let s = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
                    let ch = s[(app.animation_frame / 3) as usize % s.len()];
                    (format!("{ch}"), SUBAGENT_RUNNING)
                }
                SubagentStatus::Completed => ("✓".into(), SUBAGENT_DONE),
                SubagentStatus::Failed => ("✗".into(), SUBAGENT_FAIL),
                SubagentStatus::Cancelled => ("⊘".into(), TEXT_DIM),
            };
            lines.push(Line::from(vec![
                Span::styled(format!("    {icon} "), Style::default().fg(color)),
                Span::styled(label, Style::default().fg(TEXT_MUTED)),
            ]));
        }
    } else if app.is_busy()
        && app.active.as_ref().map_or(true, |a| !a.has_content())
        && pending_preview.is_empty()
    {
        lines.push(Line::from(""));
        let label = match app.agent_state {
            AgentState::Summarizing => "summarizing context…",
            _ => "thinking…",
        };
        lines.push(Line::from(Span::styled(
            format!("  {} {label}", app.spinner_char()),
            Style::default().fg(TEXT_DIM),
        )));
    }

    app.total_lines = lines.len();
    app.clamp_scroll(inner_height);

    let visible_start = app.scroll_offset;
    let visible_end = (visible_start + inner_height).min(lines.len());
    let visible: Vec<Line<'static>> = if visible_start < lines.len() {
        lines[visible_start..visible_end].to_vec()
    } else {
        Vec::new()
    };

    let title = transcript_title(app);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(BORDER_DIM))
        .title(Span::styled(title, Style::default().fg(TEXT_MUTED)))
        .style(Style::default().bg(TRANSCRIPT_BG));

    let paragraph = Paragraph::new(Text::from(visible)).block(block);
    f.render_widget(paragraph, area);

    if lines.len() > inner_height {
        let mut scrollbar_state = ScrollbarState::new(lines.len().saturating_sub(inner_height))
            .position(app.scroll_offset);
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(Some("▲"))
            .end_symbol(Some("▼"))
            .track_symbol(Some("│"))
            .thumb_symbol("█")
            .track_style(Style::default().fg(BORDER_DIM))
            .thumb_style(Style::default().fg(TEXT_MUTED));
        let scrollbar_area = Rect {
            x: area.x + area.width - 1,
            y: area.y + 1,
            width: 1,
            height: area.height.saturating_sub(2),
        };
        f.render_stateful_widget(scrollbar, scrollbar_area, &mut scrollbar_state);
    }
}

fn render_sidebar(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(BORDER_DIM))
        .title(Span::styled(
            " Agents ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(SIDEBAR_BG));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let inner_w = inner.width as usize;
    let mut lines: Vec<Line<'static>> = Vec::new();

    let running = app.running_subagent_count();
    let total = app.subagents.len();

    if total == 0 {
        lines.push(Line::from(Span::styled(
            " No subagents",
            Style::default().fg(TEXT_DIM),
        )));
    } else {
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {running}"),
                Style::default().fg(if running > 0 {
                    SUBAGENT_RUNNING
                } else {
                    TEXT_MUTED
                }),
            ),
            Span::styled(format!("/{total} running"), Style::default().fg(TEXT_MUTED)),
        ]));
        lines.push(Line::from(""));

        for info in app.subagents.values() {
            let (glyph, color) = match info.status {
                SubagentStatus::Running => (app.spinner_char().to_string(), SUBAGENT_RUNNING),
                SubagentStatus::Completed => ("✓".to_string(), SUBAGENT_DONE),
                SubagentStatus::Failed => ("✗".to_string(), SUBAGENT_FAIL),
                SubagentStatus::Cancelled => ("⊘".to_string(), TEXT_DIM),
            };

            let max_label = inner_w.saturating_sub(4);
            let label: String = info.label.chars().take(max_label).collect();

            lines.push(Line::from(vec![
                Span::styled(format!(" {glyph} "), Style::default().fg(color)),
                Span::styled(
                    label,
                    Style::default().fg(if info.status == SubagentStatus::Running {
                        TEXT_PRIMARY
                    } else {
                        TEXT_MUTED
                    }),
                ),
            ]));

            if let Some(model) = subagent_model_label(&info.model, &app.model) {
                let model: String = model.chars().take(inner_w.saturating_sub(10)).collect();
                lines.push(Line::from(Span::styled(
                    format!("   model {model}"),
                    Style::default().fg(TEXT_DIM),
                )));
            }

            if info.status == SubagentStatus::Running {
                let elapsed = info.started_at.elapsed();
                let elapsed_str = super::app::format_elapsed(elapsed);
                lines.push(Line::from(Span::styled(
                    format!("   {elapsed_str}"),
                    Style::default().fg(TEXT_DIM),
                )));
                if let Some(last_action) = info.actions.last() {
                    let truncated: String = last_action
                        .chars()
                        .take(inner_w.saturating_sub(4))
                        .collect();
                    lines.push(Line::from(Span::styled(
                        format!("   {truncated}"),
                        Style::default().fg(TEXT_DIM),
                    )));
                }
            }
            lines.push(Line::from(""));
        }
    }

    let visible_count = inner.height as usize;
    let visible: Vec<Line<'static>> = lines.into_iter().take(visible_count).collect();
    let paragraph = Paragraph::new(Text::from(visible));
    f.render_widget(paragraph, inner);
}

fn transcript_title(app: &App) -> String {
    let msg_count = app
        .history
        .iter()
        .filter(|e| matches!(e, HistoryEntry::User(_)))
        .count()
        + app.session_msg_count;
    if msg_count == 0 {
        " conversation ".to_string()
    } else {
        format!(" conversation · {msg_count} messages ")
    }
}

fn wrap_text(text: &str, width: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= width {
        return vec![text.to_string()];
    }
    let mut result = Vec::new();
    let mut start = 0;
    while start < chars.len() {
        let end = (start + width).min(chars.len());
        result.push(chars[start..end].iter().collect());
        start = end;
    }
    result
}

fn push_blank(lines: &mut Vec<Line<'static>>) {
    let is_last_blank = lines
        .last()
        .map(|l| l.spans.is_empty() || l.spans.iter().all(|s| s.content.is_empty()))
        .unwrap_or(true);
    if !is_last_blank {
        lines.push(Line::from(""));
    }
}

fn build_transcript_lines(app: &App, width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let w = width.saturating_sub(4).max(1);

    for entry in &app.history {
        match entry {
            HistoryEntry::User(text) => {
                push_blank(&mut lines);
                let display = if text.chars().count() > 200 {
                    let truncated: String = text.chars().take(200).collect();
                    format!("{truncated}…")
                } else {
                    text.clone()
                };
                let user_lines: Vec<&str> = display.lines().collect();
                for (li, uline) in user_lines.iter().enumerate() {
                    if li == 0 {
                        lines.push(Line::from(vec![
                            Span::styled(
                                "  › ",
                                Style::default().fg(USER_FG).add_modifier(Modifier::BOLD),
                            ),
                            Span::styled((*uline).to_string(), Style::default().fg(USER_FG)),
                        ]));
                    } else {
                        lines.push(Line::from(vec![
                            Span::raw("    "),
                            Span::styled((*uline).to_string(), Style::default().fg(USER_FG)),
                        ]));
                    }
                }
            }
            HistoryEntry::Assistant { content, reasoning } => {
                render_reasoning_block(&mut lines, reasoning.as_deref());
                push_blank(&mut lines);
                let md_lines = markdown::markdown_to_lines(content, w);
                for ml in md_lines {
                    let mut prefixed = vec![Span::raw("  ")];
                    prefixed.extend(ml.spans);
                    lines.push(Line::from(prefixed));
                }
            }
            HistoryEntry::Thinking(content) => {
                lines.push(Line::from(vec![
                    Span::styled("  ▼ ", Style::default().fg(TEXT_DIM)),
                    Span::styled(
                        "Thinking Process",
                        Style::default().fg(TEXT_DIM).add_modifier(Modifier::ITALIC),
                    ),
                ]));
                let wrap_width = w.saturating_sub(6).max(20);
                for tline in content.lines() {
                    for wrapped in wrap_text(tline, wrap_width) {
                        lines.push(Line::from(vec![
                            Span::styled("    ", Style::default()),
                            Span::styled(
                                wrapped,
                                Style::default().fg(TEXT_DIM).add_modifier(Modifier::ITALIC),
                            ),
                        ]));
                    }
                }
            }
            HistoryEntry::ToolCall {
                name,
                emoji,
                detail,
                diff,
                result_summary,
            } => {
                push_blank(&mut lines);
                render_tool_card(
                    &mut lines,
                    name,
                    emoji,
                    detail,
                    diff.as_ref(),
                    result_summary.as_ref(),
                    None,
                    w,
                    false,
                    0,
                );
            }
            HistoryEntry::Error(msg) => {
                push_blank(&mut lines);
                for (ei, err_line) in msg.lines().enumerate() {
                    if ei == 0 {
                        lines.push(Line::from(vec![
                            Span::styled(
                                "  ✗ ",
                                Style::default().fg(ERROR_FG).add_modifier(Modifier::BOLD),
                            ),
                            Span::styled(err_line.to_string(), Style::default().fg(ERROR_FG)),
                        ]));
                    } else {
                        lines.push(Line::from(Span::styled(
                            format!("    {err_line}"),
                            Style::default().fg(ERROR_FG),
                        )));
                    }
                }
            }
            HistoryEntry::System(msg) => {
                push_blank(&mut lines);
                lines.push(Line::from(Span::styled(
                    format!("  {msg}"),
                    Style::default().fg(TEXT_DIM),
                )));
            }
            HistoryEntry::Separator { summary } => {
                render_turn_separator(&mut lines, summary, w);
            }
            HistoryEntry::SubagentCard {
                task_id,
                label,
                model,
                status,
                actions,
                result_preview,
            } => {
                let (live_status, live_actions, live_preview, live_model) =
                    if let Some(info) = app.subagents.get(task_id) {
                        (
                            &info.status,
                            &info.actions,
                            info.result_preview.as_deref(),
                            info.model.as_str(),
                        )
                    } else {
                        (status, actions, result_preview.as_deref(), model.as_str())
                    };
                push_blank(&mut lines);
                render_subagent_card(
                    &mut lines,
                    label,
                    subagent_model_label(live_model, &app.model),
                    live_status,
                    live_actions,
                    live_preview,
                    w,
                    app.animation_frame,
                );
            }
        }
    }

    lines
}

fn render_reasoning_block(lines: &mut Vec<Line<'static>>, reasoning: Option<&str>) {
    let Some(reasoning) = reasoning else { return };
    if reasoning.trim().is_empty() {
        return;
    }
    push_blank(lines);
    lines.push(Line::from(vec![
        Span::styled("  ◆ ", Style::default().fg(TEXT_DIM)),
        Span::styled(
            "reasoning",
            Style::default().fg(TEXT_DIM).add_modifier(Modifier::ITALIC),
        ),
    ]));
    let max_lines = 30;
    let total = reasoning.lines().count();
    for (i, rline) in reasoning.lines().enumerate() {
        if i >= max_lines {
            lines.push(Line::from(Span::styled(
                format!("    … ({} more lines)", total - max_lines),
                Style::default().fg(TEXT_DIM),
            )));
            break;
        }
        lines.push(Line::from(Span::styled(
            format!("    {rline}"),
            Style::default().fg(TEXT_DIM).add_modifier(Modifier::ITALIC),
        )));
    }
}

fn render_subagent_card(
    lines: &mut Vec<Line<'static>>,
    label: &str,
    model: Option<&str>,
    status: &SubagentStatus,
    actions: &[String],
    result_preview: Option<&str>,
    w: usize,
    anim_frame: u16,
) {
    let (glyph, glyph_color) = match status {
        SubagentStatus::Running => {
            let spinner = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
            let ch = spinner[(anim_frame / 3) as usize % spinner.len()];
            (format!("{ch}"), SUBAGENT_RUNNING)
        }
        SubagentStatus::Completed => ("✓".to_string(), SUBAGENT_DONE),
        SubagentStatus::Failed => ("✗".to_string(), SUBAGENT_FAIL),
        SubagentStatus::Cancelled => ("⊘".to_string(), TEXT_DIM),
    };

    // Header: "  ◐ ◐ label ────"  Bottom: "    └────"
    let header_prefix_len = 4 + 2 + label.chars().count() + 1;
    let card_total = w.saturating_sub(2);
    let header_fill = "─".repeat(card_total.saturating_sub(header_prefix_len));
    let bottom_fill = "─".repeat(card_total.saturating_sub(5));

    lines.push(Line::from(vec![
        Span::styled(format!("  {glyph} "), Style::default().fg(glyph_color)),
        Span::styled("◐ ", Style::default().fg(TOOL_FG)),
        Span::styled(
            format!("{label} "),
            Style::default()
                .fg(TEXT_PRIMARY)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(header_fill, Style::default().fg(BORDER_DIM)),
    ]));

    if let Some(model) = model {
        lines.push(Line::from(vec![
            Span::styled("    │ ", Style::default().fg(BORDER_DIM)),
            Span::styled(format!("model {model}"), Style::default().fg(TEXT_DIM)),
        ]));
    }

    for action in actions.iter().rev().take(3).rev() {
        let truncated: String = action.chars().take(w.saturating_sub(8)).collect();
        lines.push(Line::from(vec![
            Span::styled("    │ ", Style::default().fg(BORDER_DIM)),
            Span::styled(truncated, Style::default().fg(TEXT_DIM)),
        ]));
    }

    if let Some(preview) = result_preview {
        let first_line: String = preview
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(w.saturating_sub(8))
            .collect();
        let color = if *status == SubagentStatus::Failed {
            SUBAGENT_FAIL
        } else {
            TEXT_MUTED
        };
        lines.push(Line::from(vec![
            Span::styled("    │ ", Style::default().fg(BORDER_DIM)),
            Span::styled(first_line, Style::default().fg(color)),
        ]));
    }

    lines.push(Line::from(Span::styled(
        format!("    └{bottom_fill}"),
        Style::default().fg(BORDER_DIM),
    )));
}

fn build_active_lines(
    active: &ActiveStreaming,
    app: &App,
    width: usize,
    anim_frame: u16,
) -> Vec<Line<'static>> {
    let w = width.saturating_sub(4).max(1);
    let mut lines = Vec::new();
    let seg_count = active.segments.len();

    for (i, seg) in active.segments.iter().enumerate() {
        let is_last = i == seg_count - 1;
        match seg {
            StreamSegment::Text(content) => {
                if !content.is_empty() {
                    push_blank(&mut lines);
                    let md_lines = markdown::markdown_to_lines(content, w);
                    for ml in md_lines {
                        let mut prefixed = vec![Span::raw("  ")];
                        prefixed.extend(ml.spans);
                        lines.push(Line::from(prefixed));
                    }
                    if is_last {
                        lines.push(Line::from(Span::styled("  ▍", Style::default().fg(ACCENT))));
                    }
                }
            }
            StreamSegment::Thinking(content) => {
                lines.push(Line::from(vec![
                    Span::styled("  ▼ ", Style::default().fg(TEXT_DIM)),
                    Span::styled(
                        "Thinking Process",
                        Style::default().fg(TEXT_DIM).add_modifier(Modifier::ITALIC),
                    ),
                ]));
                let wrap_width = w.saturating_sub(6).max(20);
                for tline in content.lines() {
                    for wrapped in wrap_text(tline, wrap_width) {
                        lines.push(Line::from(vec![
                            Span::styled("    ", Style::default()),
                            Span::styled(
                                wrapped,
                                Style::default().fg(TEXT_DIM).add_modifier(Modifier::ITALIC),
                            ),
                        ]));
                    }
                }
            }
            StreamSegment::Tool(tool) => {
                push_blank(&mut lines);
                let countdown = if is_last {
                    tool.timeout_secs.map(|total| {
                        let elapsed = tool.started_at.elapsed().as_secs();
                        total.saturating_sub(elapsed)
                    })
                } else {
                    None
                };
                render_tool_card(
                    &mut lines,
                    &tool.name,
                    &tool.emoji,
                    &tool.detail,
                    tool.diff.as_ref(),
                    tool.result_summary.as_ref(),
                    countdown,
                    w,
                    is_last,
                    anim_frame,
                );
            }
            StreamSegment::Subagent { task_id, label } => {
                let info = app.subagents.get(task_id);
                let status = info
                    .map(|i| i.status.clone())
                    .unwrap_or(SubagentStatus::Running);
                let actions: Vec<String> = info.map(|i| i.actions.clone()).unwrap_or_default();
                let preview = info.and_then(|i| i.result_preview.clone());
                let model = info.and_then(|i| subagent_model_label(&i.model, &app.model));
                push_blank(&mut lines);
                render_subagent_card(
                    &mut lines,
                    label,
                    model,
                    &status,
                    &actions,
                    preview.as_deref(),
                    w,
                    anim_frame,
                );
            }
        }
    }

    lines
}

fn should_show_subagent_model(subagent_model: &str, main_model: &str) -> bool {
    let subagent_model = subagent_model.trim();
    !subagent_model.is_empty() && subagent_model != main_model.trim()
}

fn subagent_model_label<'a>(subagent_model: &'a str, main_model: &str) -> Option<&'a str> {
    should_show_subagent_model(subagent_model, main_model).then_some(subagent_model.trim())
}

fn distinct_subagent_models(app: &App) -> Vec<String> {
    app.configured_subagent_model
        .iter()
        .filter_map(|model| subagent_model_label(model, &app.model))
        .chain(
            app.subagents
                .values()
                .filter_map(|info| subagent_model_label(&info.model, &app.model)),
        )
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(ToOwned::to_owned)
        .collect()
}

#[cfg(test)]
pub mod tests {
    use std::path::PathBuf;

    use super::super::app::EngineEvent;
    use super::*;

    #[test]
    fn distinct_subagent_models_keeps_same_provider_different_model_visible() {
        let mut app = App::new(
            "Qwen3.6-27B-FP8".into(),
            "vllmrs".into(),
            PathBuf::from("/tmp"),
            0,
            "0/262144".into(),
            None,
        );

        app.handle_engine_event(EngineEvent::SubagentStarted {
            task_id: "sub1".into(),
            label: "scan bugs".into(),
            task: "scan folder".into(),
            model: "Qwen3.5-35B-A3B-FP8".into(),
        });

        assert_eq!(app.model, "Qwen3.6-27B-FP8");
        assert_eq!(
            distinct_subagent_models(&app),
            vec!["Qwen3.5-35B-A3B-FP8".to_string()]
        );
    }

    #[test]
    fn distinct_subagent_models_hides_inherited_model() {
        let mut app = App::new(
            "Qwen3.6-27B-FP8".into(),
            "vllmrs".into(),
            PathBuf::from("/tmp"),
            0,
            "0/262144".into(),
            None,
        );

        app.handle_engine_event(EngineEvent::SubagentStarted {
            task_id: "sub1".into(),
            label: "scan bugs".into(),
            task: "scan folder".into(),
            model: "Qwen3.6-27B-FP8".into(),
        });

        assert!(distinct_subagent_models(&app).is_empty());
    }

    #[test]
    fn distinct_subagent_models_uses_configured_model_before_any_subagent_starts() {
        let app = App::new(
            "Qwen3.6-27B-FP8".into(),
            "vllmrs".into(),
            PathBuf::from("/tmp"),
            0,
            "0/262144".into(),
            Some("Qwen3.5-35B-A3B-FP8".into()),
        );

        assert_eq!(
            distinct_subagent_models(&app),
            vec!["Qwen3.5-35B-A3B-FP8".to_string()]
        );
    }

    #[test]
    fn test_compute_cursor_position_multiline_wrap() {
        // Test cursor position with word wrapping
        let input = "hello world";
        let width = 5;

        // Position at start
        let (x, y) = compute_cursor_position(input, 0, width);
        assert_eq!((x, y), (0, 0));

        // Position after "hello" (exact width, no wrap)
        let (x, y) = compute_cursor_position(input, 5, width);
        assert_eq!((x, y), (5, 0));

        // Position at end
        let (x, y) = compute_cursor_position(input, 11, width);
        assert_eq!((x, y), (1, 2));
    }

    #[test]
    fn test_compute_cursor_position_with_newlines_and_wrap() {
        // Multi-line with word wrapping
        let input = "hello\nworld";
        let width = 5;

        // Position at "hello" (exact width, no wrap)
        let (x, y) = compute_cursor_position(input, 5, width);
        assert_eq!((x, y), (5, 0));

        // Position at end
        let (x, y) = compute_cursor_position(input, 11, width);
        assert_eq!((x, y), (5, 1));
    }

    #[test]
    fn test_compute_cursor_position_edge_cases() {
        // Empty input
        let (x, y) = compute_cursor_position("", 0, 10);
        assert_eq!((x, y), (0, 0));

        // Single character
        let (x, y) = compute_cursor_position("a", 1, 10);
        assert_eq!((x, y), (1, 0));

        // Exact width fit
        let (x, y) = compute_cursor_position("hello", 5, 5);
        assert_eq!((x, y), (5, 0));

        // One over width
        let (x, y) = compute_cursor_position("hello", 6, 5);
        assert_eq!((x, y), (1, 1));
    }
}

fn render_tool_card(
    lines: &mut Vec<Line<'static>>,
    name: &str,
    emoji: &str,
    detail: &str,
    diff: Option<&EditDiff>,
    result_summary: Option<&(bool, String)>,
    countdown: Option<u64>,
    w: usize,
    running: bool,
    anim_frame: u16,
) {
    let (glyph, verb) = tool_family(name);
    let display_name = if verb == "tool" {
        name.to_string()
    } else {
        verb.to_string()
    };
    let (status_sym, status_color) = if running {
        let spinner = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
        let ch = spinner[(anim_frame / 3) as usize % spinner.len()];
        (format!("{ch}"), WORKING_FG)
    } else {
        ("✓".to_string(), SUCCESS_FG)
    };

    // Header: "  ✓ ◆ write ────"  Bottom: "    └────"
    let header_prefix_len = 4 + 2 + display_name.chars().count() + 1;
    let card_total = w.saturating_sub(2);
    let header_fill = "─".repeat(card_total.saturating_sub(header_prefix_len));
    let bottom_fill = "─".repeat(card_total.saturating_sub(5));

    lines.push(Line::from(vec![
        Span::styled(
            format!("  {status_sym} "),
            Style::default().fg(status_color),
        ),
        Span::styled(format!("{glyph} "), Style::default().fg(TOOL_FG)),
        Span::styled(
            format!("{display_name} "),
            Style::default()
                .fg(TEXT_PRIMARY)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(header_fill, Style::default().fg(BORDER_DIM)),
    ]));

    if !detail.is_empty() {
        for dline in detail.lines() {
            lines.push(Line::from(vec![
                Span::styled("    │ ", Style::default().fg(BORDER_DIM)),
                Span::styled(
                    truncate_end(dline, w.saturating_sub(8)),
                    Style::default().fg(TEXT_MUTED),
                ),
            ]));
        }
    } else if !running {
        lines.push(Line::from(vec![
            Span::styled("    │ ", Style::default().fg(BORDER_DIM)),
            Span::styled(format!("{emoji} {name}"), Style::default().fg(TEXT_MUTED)),
        ]));
    }

    if let Some(remaining) = countdown {
        let mins = remaining / 60;
        let secs = remaining % 60;
        let timer_text = if mins > 0 {
            format!("⏱ {mins}m {secs:02}s remaining")
        } else {
            format!("⏱ {secs}s remaining")
        };
        lines.push(Line::from(vec![
            Span::styled("    │ ", Style::default().fg(BORDER_DIM)),
            Span::styled(timer_text, Style::default().fg(WORKING_FG)),
        ]));
    }

    if let Some(diff) = diff {
        render_edit_diff(lines, diff, w);
    }

    if let Some((success, summary)) = result_summary {
        let (icon, color) = if *success {
            ("→", TEXT_DIM)
        } else {
            ("✗", ERROR_FG)
        };
        for (i, sline) in summary.lines().enumerate() {
            let prefix = if i == 0 {
                format!("    │ {icon} ")
            } else {
                "    │   ".to_string()
            };
            let text = truncate_end(sline, w.saturating_sub(prefix.len()));
            lines.push(Line::from(vec![
                Span::styled(prefix, Style::default().fg(BORDER_DIM)),
                Span::styled(text, Style::default().fg(color)),
            ]));
        }
    }

    lines.push(Line::from(Span::styled(
        format!("    └{bottom_fill}"),
        Style::default().fg(BORDER_DIM),
    )));
}

fn render_edit_diff(lines: &mut Vec<Line<'static>>, diff: &EditDiff, w: usize) {
    use rbot::diff::DiffKind;

    let max_w = w.saturating_sub(8);
    lines.push(Line::from(vec![
        Span::styled("    │ ", Style::default().fg(BORDER_DIM)),
        Span::styled(
            truncate_end(&diff.path, max_w),
            Style::default()
                .fg(DIFF_HEADER)
                .add_modifier(Modifier::BOLD),
        ),
    ]));

    lines.push(Line::from(vec![
        Span::styled("    │ ", Style::default().fg(BORDER_DIM)),
        Span::styled(
            format!("{:>5} {:>5} │ ", "old", "new"),
            Style::default().fg(TEXT_DIM),
        ),
    ]));

    let max_diff_lines = 24;
    for dl in diff.lines.iter().take(max_diff_lines) {
        let old_no = dl
            .old_lineno
            .map(|n| format!("{n:>5}"))
            .unwrap_or_else(|| "     ".to_string());
        let new_no = dl
            .new_lineno
            .map(|n| format!("{n:>5}"))
            .unwrap_or_else(|| "     ".to_string());
        let gutter = format!(" {old_no} {new_no} │ {} ", dl.marker);
        let text_w = max_w.saturating_sub(gutter.chars().count());
        let content = truncate_end(&dl.text, text_w);

        let fg = match dl.kind {
            DiffKind::Added => DIFF_ADD,
            DiffKind::Removed => DIFF_DEL,
            DiffKind::Context => TEXT_DIM,
            DiffKind::Omitted => TEXT_DIM,
        };
        let bg = match dl.kind {
            DiffKind::Added => Some(Color::Rgb(20, 40, 20)),
            DiffKind::Removed => Some(Color::Rgb(50, 20, 20)),
            DiffKind::Context | DiffKind::Omitted => None,
        };

        lines.push(Line::from(vec![
            Span::styled("    │ ", Style::default().fg(BORDER_DIM)),
            Span::styled(gutter, Style::default().fg(TEXT_DIM)),
            if let Some(bg) = bg {
                Span::styled(content, Style::default().fg(fg).bg(bg))
            } else {
                Span::styled(content, Style::default().fg(fg))
            },
        ]));
    }

    if diff.lines.len() > max_diff_lines {
        lines.push(Line::from(vec![
            Span::styled("    │ ", Style::default().fg(BORDER_DIM)),
            Span::styled(
                format!("  … {} more lines", diff.lines.len() - max_diff_lines),
                Style::default().fg(TEXT_DIM),
            ),
        ]));
    }
}

fn render_turn_separator(
    lines: &mut Vec<Line<'static>>,
    summary: &super::app::TurnSummary,
    w: usize,
) {
    lines.push(Line::from(""));
    let status = format_summary(summary);
    let status_len = status.chars().count();
    let left = w.saturating_sub(status_len + 4) / 2;
    let right = w.saturating_sub(status_len + 4 + left);
    let sep_left = "─".repeat(left.min(30));
    let sep_right = "─".repeat(right.min(30));
    lines.push(Line::from(vec![
        Span::styled(format!("  {sep_left} "), Style::default().fg(SEPARATOR_FG)),
        Span::styled(status, Style::default().fg(ACCENT)),
        Span::styled(format!(" {sep_right}"), Style::default().fg(SEPARATOR_FG)),
    ]));
}

fn render_composer(f: &mut Frame, area: Rect, app: &App) {
    let busy = app.is_busy();
    let border_color = if busy { TEXT_DIM } else { ACCENT };
    let title = if busy {
        let state_label = match app.agent_state {
            AgentState::Summarizing => "summarizing…",
            AgentState::WaitingSubagents => "waiting agents…",
            _ => "working…",
        };
        if app.pending.is_empty() {
            format!(" {} {state_label} ", app.spinner_char())
        } else {
            format!(
                " {} {state_label} ({} queued) ",
                app.spinner_char(),
                app.pending.len()
            )
        }
    } else if app.composer.input.contains('\n') {
        " draft · Enter=send  Ctrl+J=newline ".to_string()
    } else {
        " message · Enter=send ".to_string()
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .title(Span::styled(
            title,
            Style::default().fg(if busy { TEXT_DIM } else { TEXT_MUTED }),
        ))
        .style(Style::default().bg(COMPOSER_BG));

    let inner = block.inner(area);

    let display_text = if app.composer.input.is_empty() && !busy {
        Text::from(Span::styled(
            "Type a message… (↑ history, ? help)",
            Style::default().fg(TEXT_DIM).add_modifier(Modifier::ITALIC),
        ))
    } else {
        Text::from(app.composer.input.as_str().to_string())
    };

    let paragraph = Paragraph::new(display_text)
        .block(block)
        .wrap(Wrap { trim: false });
    f.render_widget(paragraph, area);

    if !app.show_help && !busy {
        let (cursor_x, cursor_y) = compute_cursor_position(
            &app.composer.input,
            app.composer.cursor,
            inner.width as usize,
        );
        let cx = inner.x + cursor_x as u16;
        let cy = inner.y + cursor_y as u16;
        if cy < inner.y + inner.height {
            f.set_cursor_position((cx, cy));
        }
    }
}

pub(crate) fn compute_cursor_position(input: &str, cursor: usize, width: usize) -> (usize, usize) {
    let before: String = input.chars().take(cursor).collect();
    let mut x = 0usize;
    let mut y = 0usize;
    for ch in before.chars() {
        if ch == '\n' {
            x = 0;
            y += 1;
        } else {
            x += 1;
            if width > 0 && x > width {
                x = 1;
                y += 1;
            }
        }
    }
    (x, y)
}

fn render_footer(f: &mut Frame, area: Rect, app: &App) {
    let status = app.status_line();
    let busy = app.is_busy();
    let shortcuts = if busy {
        "Ctrl+C:cancel  ↑↓:history  Shift+↑↓:scroll  Alt+4:agents  ?:help"
    } else {
        "Enter:send  Ctrl+C:quit  ↑↓:history  Alt+4:agents  ?:help"
    };

    let available = area.width as usize;
    let right_len = status.chars().count();
    let sep = " │ ";
    let left_space = available.saturating_sub(right_len + sep.len() + 2);

    let left_text = if shortcuts.len() > left_space {
        &shortcuts[..left_space.min(shortcuts.len())]
    } else {
        shortcuts
    };

    let padding = available.saturating_sub(left_text.len() + sep.len() + right_len + 2);

    let status_color = match app.agent_state {
        AgentState::Working => WORKING_FG,
        AgentState::WaitingSubagents => SUBAGENT_RUNNING,
        AgentState::Summarizing => SUMMARIZE_FG,
        AgentState::Ready => ACCENT,
    };

    let spans = vec![
        Span::styled(format!(" {left_text}"), Style::default().fg(TEXT_DIM)),
        Span::raw(" ".repeat(padding)),
        Span::styled(sep, Style::default().fg(BORDER_DIM)),
        Span::styled(format!("{status} "), Style::default().fg(status_color)),
    ];
    let footer = Paragraph::new(Line::from(spans)).style(Style::default().bg(HEADER_BG));
    f.render_widget(footer, area);
}

fn render_help_overlay(f: &mut Frame, area: Rect) {
    let width = area.width.min(64);
    let height = area.height.min(32);
    let x = (area.width.saturating_sub(width)) / 2;
    let y = (area.height.saturating_sub(height)) / 2;
    let popup = Rect::new(x, y, width, height);

    f.render_widget(Clear, popup);

    let sections: Vec<(&str, Vec<(&str, &str)>)> = vec![
        (
            "Editing",
            vec![
                ("Enter", "Send message"),
                ("Ctrl+J / Alt+Enter", "Insert newline"),
                ("Ctrl+C", "Cancel turn or quit"),
                ("Ctrl+D", "Quit (empty input)"),
                ("Esc", "Clear input"),
                ("Ctrl+U", "Clear entire draft"),
                ("Ctrl+W", "Delete word backward"),
                ("Ctrl+A / Home", "Start of line"),
                ("Ctrl+E / End", "End of line"),
            ],
        ),
        (
            "Navigation",
            vec![
                ("↑ / ↓", "Browse input history"),
                ("Shift+↑ / Shift+↓", "Scroll transcript"),
                ("PgUp / PgDn", "Page scroll"),
                ("Mouse wheel", "Scroll transcript"),
            ],
        ),
        (
            "Agents & Commands",
            vec![
                ("Alt+4", "Toggle agents sidebar"),
                ("/agents", "Toggle agents sidebar"),
                ("/help or ?", "Toggle this help"),
                ("/clear", "Clear & reset session"),
                ("/exit or /quit", "Exit rbot"),
                ("/stop", "Cancel current turn"),
                ("/new", "Start new session"),
                ("/model [name]", "Switch or show model"),
                ("/memorize <text>", "Save to memory"),
                ("/status", "Show session status"),
            ],
        ),
    ];

    let mut text_lines: Vec<Line<'static>> = Vec::new();
    for (section, bindings) in &sections {
        if !text_lines.is_empty() {
            text_lines.push(Line::from(""));
        }
        text_lines.push(Line::from(Span::styled(
            format!("  {section}"),
            Style::default()
                .fg(ACCENT)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        )));
        for (key, desc) in bindings {
            text_lines.push(Line::from(vec![
                Span::styled(
                    format!("  {key:<24}"),
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ),
                Span::styled(desc.to_string(), Style::default().fg(TEXT_PRIMARY)),
            ]));
        }
    }

    let block = Block::default()
        .title(Span::styled(
            " Keyboard Shortcuts ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ))
        .title_alignment(Alignment::Center)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACCENT))
        .style(Style::default().bg(HEADER_BG));

    let help = Paragraph::new(Text::from(text_lines)).block(block);
    f.render_widget(help, popup);
}

fn render_approval_overlay(f: &mut Frame, area: Rect, app: &super::app::App) {
    use rbot::diff::DiffKind;

    let dialog = match &app.approval_dialog {
        Some(d) => d,
        None => return,
    };

    let width = area.width.min(80);
    let height = area.height.min(28);
    let x = (area.width.saturating_sub(width)) / 2;
    let y = (area.height.saturating_sub(height)) / 2;
    let popup = Rect::new(x, y, width, height);

    f.render_widget(Clear, popup);

    let inner_w = width.saturating_sub(4) as usize;
    let mut text_lines: Vec<Line<'static>> = Vec::new();

    text_lines.push(Line::from(vec![
        Span::styled("  File: ", Style::default().fg(TEXT_DIM)),
        Span::styled(
            truncate_end(&dialog.path, inner_w.saturating_sub(8)),
            Style::default()
                .fg(DIFF_HEADER)
                .add_modifier(Modifier::BOLD),
        ),
    ]));
    text_lines.push(Line::from(""));

    let max_lines = height.saturating_sub(8) as usize;
    for dl in dialog.diff_lines.iter().take(max_lines) {
        let old_no = dl
            .old_lineno
            .map(|n| format!("{n:>4}"))
            .unwrap_or_else(|| "    ".to_string());
        let new_no = dl
            .new_lineno
            .map(|n| format!("{n:>4}"))
            .unwrap_or_else(|| "    ".to_string());
        let gutter = format!(" {old_no} {new_no} {} ", dl.marker);
        let text_w = inner_w.saturating_sub(gutter.chars().count());
        let content = truncate_end(&dl.text, text_w);

        let fg = match dl.kind {
            DiffKind::Added => DIFF_ADD,
            DiffKind::Removed => DIFF_DEL,
            DiffKind::Context => TEXT_DIM,
            DiffKind::Omitted => TEXT_DIM,
        };
        let bg = match dl.kind {
            DiffKind::Added => Some(Color::Rgb(20, 40, 20)),
            DiffKind::Removed => Some(Color::Rgb(50, 20, 20)),
            DiffKind::Context | DiffKind::Omitted => None,
        };

        text_lines.push(Line::from(vec![
            Span::styled(gutter, Style::default().fg(TEXT_DIM)),
            if let Some(bg) = bg {
                Span::styled(content, Style::default().fg(fg).bg(bg))
            } else {
                Span::styled(content, Style::default().fg(fg))
            },
        ]));
    }

    if dialog.diff_lines.len() > max_lines {
        text_lines.push(Line::from(Span::styled(
            format!("  … {} more lines", dialog.diff_lines.len() - max_lines),
            Style::default().fg(TEXT_DIM),
        )));
    }

    text_lines.push(Line::from(""));

    let options = ["Allow Once", "Always Allow", "Deny"];
    let option_line = options
        .iter()
        .enumerate()
        .map(|(i, label)| {
            if i == dialog.selected {
                Span::styled(
                    format!(" [{label}] "),
                    Style::default()
                        .fg(Color::Black)
                        .bg(ACCENT)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                Span::styled(format!("  {label}  "), Style::default().fg(TEXT_PRIMARY))
            }
        })
        .collect::<Vec<_>>();
    text_lines.push(Line::from(option_line));
    text_lines.push(Line::from(Span::styled(
        "  ←/→ select · Enter confirm",
        Style::default().fg(TEXT_DIM),
    )));

    let title = format!(" Approve {} ", dialog.tool_name);
    let block = Block::default()
        .title(Span::styled(
            title,
            Style::default()
                .fg(Color::Rgb(255, 200, 80))
                .add_modifier(Modifier::BOLD),
        ))
        .title_alignment(Alignment::Center)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Rgb(255, 200, 80)))
        .style(Style::default().bg(HEADER_BG));

    let paragraph = Paragraph::new(Text::from(text_lines)).block(block);
    f.render_widget(paragraph, popup);
}

fn tool_family(name: &str) -> (&'static str, &'static str) {
    match name {
        "read_file" | "list_dir" | "list_files" | "grep_files" => ("▷", "read"),
        "write_file" => ("◆", "write"),
        "edit_file" => ("✎", "patch"),
        "exec" => ("▶", "run"),
        "web_search" | "web_fetch" => ("⌕", "search"),
        "spawn" => ("◐", "agent"),
        "cron" => ("⏱", "schedule"),
        _ => ("⋮", "tool"),
    }
}

fn format_summary(s: &super::app::TurnSummary) -> String {
    let mut parts = vec!["✓ done".to_string()];
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
    parts.push(super::app::format_elapsed(s.elapsed));
    parts.join(" · ")
}

fn truncate_end(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        text.to_string()
    } else if max <= 1 {
        "…".to_string()
    } else {
        let mut out: String = text.chars().take(max - 1).collect();
        out.push('…');
        out
    }
}
