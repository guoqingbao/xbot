use std::collections::BTreeSet;

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::app::{
    ActiveStreaming, AgentState, App, EditDiff, HistoryEntry, ScrollbarGeometry, StreamSegment,
    SubagentStatus, SubagentTokenUsage,
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
const SESSION_HINT_FG: Color = Color::Rgb(120, 205, 255);

const SIDEBAR_MIN_WIDTH: u16 = 100;
const SIDEBAR_WIDTH: u16 = 28;

pub fn render(f: &mut Frame, app: &mut App) {
    let area = f.area();

    let composer_h = composer_height(&app.composer.input, app.composer.cursor, area.width);
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

    if app.show_subagent_overlay {
        render_subagent_overlay(f, area, app);
    }

    if app.show_session_overlay {
        render_session_overlay(f, area, app);
    }
}

fn composer_height(input: &str, cursor: usize, area_width: u16) -> u16 {
    let inner_w = area_width.saturating_sub(2).max(1) as usize;
    let visible_lines = wrap_hard_lines(input, inner_w).len();
    let (_, cursor_y) = compute_cursor_position(input, cursor, inner_w);
    (visible_lines.max(cursor_y + 1) as u16 + 2).min(12).max(3)
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
            " ■ xbot ",
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
        && app.line_buffer.pending_preview().is_empty()
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
        let max_scroll = lines.len().saturating_sub(inner_height);
        let mut scrollbar_state = ScrollbarState::new(max_scroll.saturating_add(1))
            .position(app.scroll_offset)
            .viewport_content_length(inner_height);
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
        let track_y = scrollbar_area.y.saturating_add(1);
        let track_height = scrollbar_area.height.saturating_sub(2);
        let thumb_length = ((track_height as usize * inner_height) / lines.len())
            .max(1)
            .min(track_height as usize) as u16;
        let travel = track_height.saturating_sub(thumb_length) as usize;
        let thumb_start = track_y
            .saturating_add((travel.saturating_mul(app.scroll_offset) / max_scroll.max(1)) as u16);
        app.set_scrollbar_geometry(Some(ScrollbarGeometry {
            x: scrollbar_area.x,
            y: track_y,
            height: track_height,
            thumb_start,
            thumb_length,
            max_scroll,
        }));
        f.render_stateful_widget(scrollbar, scrollbar_area, &mut scrollbar_state);
    } else {
        app.set_scrollbar_geometry(None);
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
            } else if let Some(finished_at) = info.finished_at {
                let duration = finished_at.saturating_duration_since(info.started_at);
                let elapsed_str = super::app::format_elapsed(duration);
                let mut hint_parts = vec![elapsed_str];
                if let Some(usage) = &info.token_usage {
                    hint_parts.push(format_subagent_token_hint(usage));
                }
                lines.push(Line::from(Span::styled(
                    format!("   {}", hint_parts.join(" · ")),
                    Style::default().fg(TEXT_DIM),
                )));
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
    if UnicodeWidthStr::width(text) <= width {
        return vec![text.to_string()];
    }
    let mut result = Vec::new();
    push_hard_wrapped(text, width, &mut result);
    result
}

fn char_display_width(ch: char) -> usize {
    if ch == '\t' {
        4
    } else {
        UnicodeWidthChar::width(ch).unwrap_or(0)
    }
}

fn push_hard_wrapped(text: &str, width: usize, out: &mut Vec<String>) {
    let width = width.max(1);
    let mut current = String::new();
    let mut current_width = 0usize;

    for ch in text.chars() {
        let ch_width = char_display_width(ch);
        if ch_width > 0 && current_width + ch_width > width {
            out.push(std::mem::take(&mut current));
            current_width = 0;
        }
        current.push(ch);
        current_width += ch_width;
    }

    out.push(current);
}

fn wrap_hard_lines(text: &str, width: usize) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.split('\n') {
        push_hard_wrapped(line, width, &mut out);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

fn wrap_words(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    if text.is_empty() {
        return vec![String::new()];
    }

    let mut result = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;
    let mut pending_space = String::new();
    let mut pending_space_width = 0usize;
    let mut word = String::new();
    let mut word_width = 0usize;

    let flush_word = |result: &mut Vec<String>,
                      current: &mut String,
                      current_width: &mut usize,
                      pending_space: &mut String,
                      pending_space_width: &mut usize,
                      word: &mut String,
                      word_width: &mut usize| {
        if word.is_empty() {
            return;
        }

        if current.is_empty() && !pending_space.is_empty() {
            if *pending_space_width <= width {
                current.push_str(pending_space);
                *current_width = *pending_space_width;
            } else {
                push_hard_wrapped(pending_space, width, result);
                if let Some(last) = result.pop() {
                    *current_width = UnicodeWidthStr::width(last.as_str());
                    *current = last;
                }
            }
            pending_space.clear();
            *pending_space_width = 0;
        }

        if !current.is_empty() && *current_width + *pending_space_width + *word_width <= width {
            current.push_str(pending_space);
            *current_width += *pending_space_width;
        } else if !current.is_empty() {
            result.push(std::mem::take(current));
            *current_width = 0;
        }

        pending_space.clear();
        *pending_space_width = 0;

        if *word_width <= width {
            current.push_str(word);
            *current_width += *word_width;
        } else {
            if !current.is_empty() {
                result.push(std::mem::take(current));
                *current_width = 0;
            }
            push_hard_wrapped(word, width, result);
            if let Some(last) = result.pop() {
                *current_width = UnicodeWidthStr::width(last.as_str());
                *current = last;
            }
        }

        word.clear();
        *word_width = 0;
    };

    for ch in text.chars() {
        let ch_width = char_display_width(ch);
        if ch.is_whitespace() {
            flush_word(
                &mut result,
                &mut current,
                &mut current_width,
                &mut pending_space,
                &mut pending_space_width,
                &mut word,
                &mut word_width,
            );
            pending_space.push(ch);
            pending_space_width += ch_width;
        } else {
            word.push(ch);
            word_width += ch_width;
        }
    }

    flush_word(
        &mut result,
        &mut current,
        &mut current_width,
        &mut pending_space,
        &mut pending_space_width,
        &mut word,
        &mut word_width,
    );

    if !current.is_empty() {
        result.push(current);
    }
    if result.is_empty() {
        result.push(String::new());
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

    for (idx, entry) in app.history.iter().enumerate() {
        match entry {
            HistoryEntry::User(text) => {
                push_blank(&mut lines);
                let mut first = true;
                for uline in text.split('\n') {
                    for wrapped in wrap_words(uline, w) {
                        if first {
                            first = false;
                            lines.push(Line::from(vec![
                                Span::styled(
                                    "  › ",
                                    Style::default().fg(USER_FG).add_modifier(Modifier::BOLD),
                                ),
                                Span::styled(wrapped, Style::default().fg(USER_FG)),
                            ]));
                        } else {
                            lines.push(Line::from(vec![
                                Span::raw("    "),
                                Span::styled(wrapped, Style::default().fg(USER_FG)),
                            ]));
                        }
                    }
                }
            }
            HistoryEntry::Assistant { content, reasoning } => {
                let follows_thinking =
                    idx > 0 && matches!(app.history[idx - 1], HistoryEntry::Thinking(_));
                let has_reasoning = reasoning
                    .as_deref()
                    .is_some_and(|text| !text.trim().is_empty());
                render_reasoning_block(&mut lines, reasoning.as_deref());
                if !follows_thinking && !has_reasoning {
                    push_blank(&mut lines);
                }
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
                let wrap_width = w.saturating_sub(6).max(20);
                for (ei, err_line) in msg.lines().enumerate() {
                    let wrapped = wrap_words(err_line, wrap_width);
                    for (wi, wl) in wrapped.into_iter().enumerate() {
                        if ei == 0 && wi == 0 {
                            lines.push(Line::from(vec![
                                Span::styled(
                                    "  ✗ ",
                                    Style::default().fg(ERROR_FG).add_modifier(Modifier::BOLD),
                                ),
                                Span::styled(wl, Style::default().fg(ERROR_FG)),
                            ]));
                        } else {
                            lines.push(Line::from(Span::styled(
                                format!("    {wl}"),
                                Style::default().fg(ERROR_FG),
                            )));
                        }
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
            HistoryEntry::SessionHint(msg) => {
                let follows_session_hint =
                    idx > 0 && matches!(app.history[idx - 1], HistoryEntry::SessionHint(_));
                if !follows_session_hint {
                    push_blank(&mut lines);
                }
                let wrap_width = w.saturating_sub(4).max(20);
                for (line_idx, wrapped) in wrap_words(msg, wrap_width).into_iter().enumerate() {
                    let prefix = if line_idx == 0 { "  ◆ " } else { "    " };
                    lines.push(Line::from(vec![
                        Span::styled(
                            prefix,
                            Style::default()
                                .fg(SESSION_HINT_FG)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            wrapped,
                            Style::default()
                                .fg(SESSION_HINT_FG)
                                .add_modifier(Modifier::BOLD),
                        ),
                    ]));
                }
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
                    if i == 0 || !matches!(active.segments[i - 1], StreamSegment::Thinking(_)) {
                        push_blank(&mut lines);
                    }
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

    fn test_app_with_model(model: &str, subagent_model: Option<&str>) -> App {
        App::new(
            model.into(),
            "vllmrs".into(),
            PathBuf::from("/tmp"),
            0,
            "0/262144".into(),
            subagent_model.map(Into::into),
            String::new(),
            "test:key".into(),
            Vec::new(),
        )
    }

    fn test_app_simple() -> App {
        App::new(
            "main".into(),
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
    fn footer_shortcuts_truncate_on_char_boundaries() {
        let shortcuts = [
            "Ctrl+C:cancel  ↑↓:scroll  Shift+↑↓:history  Ctrl/Alt+4:agents  ?:help",
            "Enter:send  Ctrl+C:quit  ↑↓:scroll  Shift+↑↓:history  Ctrl/Alt+4:agents  ?:help",
        ];

        for shortcut in shortcuts {
            for width in 0..=UnicodeWidthStr::width(shortcut) {
                let truncated = truncate_display_width(shortcut, width);
                assert!(shortcut.is_char_boundary(truncated.len()));
                assert!(UnicodeWidthStr::width(truncated.as_str()) <= width);
            }
        }
    }

    #[test]
    fn distinct_subagent_models_keeps_same_provider_different_model_visible() {
        let mut app = test_app_with_model("Qwen3.6-27B-FP8", None);

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
        let mut app = test_app_with_model("Qwen3.6-27B-FP8", None);

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
        let app = test_app_with_model("Qwen3.6-27B-FP8", Some("Qwen3.5-35B-A3B-FP8"));

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
        assert_eq!((x, y), (0, 1));

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
        assert_eq!((x, y), (0, 1));

        // Position at end
        let (x, y) = compute_cursor_position(input, 11, width);
        assert_eq!((x, y), (0, 2));
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
        assert_eq!((x, y), (0, 1));

        // One over width
        let (x, y) = compute_cursor_position("hello!", 6, 5);
        assert_eq!((x, y), (1, 1));
    }

    #[test]
    fn user_history_wraps_long_and_multiline_prompts() {
        let mut app = test_app_simple();
        app.history
            .push(HistoryEntry::User("one two three\nfour five six".into()));

        let lines = build_transcript_lines(&app, 14);
        let plain = lines
            .into_iter()
            .map(|line| {
                line.spans
                    .into_iter()
                    .map(|span| span.content.into_owned())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();

        assert_eq!(
            plain,
            vec!["  › one two", "    three", "    four five", "    six"]
        );
    }

    #[test]
    fn user_history_wrap_keeps_prompt_indentation() {
        let mut app = test_app_simple();
        app.history
            .push(HistoryEntry::User("  let value = 1;".into()));

        let lines = build_transcript_lines(&app, 14);
        let plain = lines
            .into_iter()
            .map(|line| {
                line.spans
                    .into_iter()
                    .map(|span| span.content.into_owned())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();

        assert_eq!(plain, vec!["  ›   let", "    value = 1;"]);
    }

    #[test]
    fn assistant_after_thinking_has_no_extra_blank_line() {
        let mut app = test_app_simple();
        app.history.push(HistoryEntry::Thinking("checking".into()));
        app.history.push(HistoryEntry::Assistant {
            content: "answer".into(),
            reasoning: None,
        });

        let lines = build_transcript_lines(&app, 40);
        let plain = lines
            .into_iter()
            .map(|line| {
                line.spans
                    .into_iter()
                    .map(|span| span.content.into_owned())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();

        assert_eq!(
            plain,
            vec!["  ▼ Thinking Process", "    checking", "  answer"]
        );
    }

    #[test]
    fn active_text_after_thinking_has_no_extra_blank_line() {
        let app = test_app_simple();
        let mut active = ActiveStreaming::default();
        active.push_thinking("checking");
        active.push_text("answer");

        let lines = build_active_lines(&active, &app, 40, 0);
        let plain = lines
            .into_iter()
            .map(|line| {
                line.spans
                    .into_iter()
                    .map(|span| span.content.into_owned())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();

        assert_eq!(
            plain,
            vec!["  ▼ Thinking Process", "    checking", "  answer", "  ▍"]
        );
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
    use xbot::diff::DiffKind;

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
    for sa in &summary.subagent_summaries {
        let cache_hint = if sa.cached_tokens > 0 && sa.prompt_tokens > 0 {
            let pct = (sa.cached_tokens * 100) / sa.prompt_tokens;
            format!("({}% cached) ", pct)
        } else {
            String::new()
        };
        let label: String = sa.label.chars().take(20).collect();
        let sa_line = format!(
            "    ◐ {label}: ↑{} {}↓{}",
            sa.prompt_tokens, cache_hint, sa.completion_tokens
        );
        lines.push(Line::from(Span::styled(
            sa_line,
            Style::default().fg(TEXT_DIM),
        )));
    }
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
        let has_pending = !app.pending.is_empty();
        let steer_hint = if app.steer_tx.is_some() && has_pending {
            " · Alt+S:steer"
        } else {
            ""
        };
        if !has_pending {
            format!(" {} {state_label} ", app.spinner_char())
        } else {
            format!(
                " {} {state_label} ({} queued){steer_hint} ",
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

    f.render_widget(block, area);

    if app.composer.input.is_empty() {
        let placeholder = if busy {
            "Type a follow up message..."
        } else {
            "Type a message... (↑ history, ? help)"
        };
        let placeholder = Paragraph::new(Text::from(Span::styled(
            placeholder,
            Style::default().fg(TEXT_DIM).add_modifier(Modifier::ITALIC),
        )))
        .style(Style::default().bg(COMPOSER_BG));
        f.render_widget(placeholder, inner);
    } else {
        let mut visual_lines = wrap_hard_lines(&app.composer.input, inner.width as usize);
        let (_, cursor_y) = compute_cursor_position(
            &app.composer.input,
            app.composer.cursor,
            inner.width as usize,
        );
        while visual_lines.len() <= cursor_y {
            visual_lines.push(String::new());
        }

        let visible_height = inner.height as usize;
        let start = cursor_y.saturating_add(1).saturating_sub(visible_height);
        let visible = visual_lines
            .into_iter()
            .skip(start)
            .take(visible_height)
            .map(|line| Line::from(Span::styled(line, Style::default().fg(TEXT_PRIMARY))))
            .collect::<Vec<_>>();
        let paragraph = Paragraph::new(Text::from(visible));
        let paragraph = paragraph.style(Style::default().bg(COMPOSER_BG));
        f.render_widget(paragraph, inner);
    }

    if !app.show_help && app.approval_dialog.is_none() {
        let (cursor_x, cursor_y) = compute_cursor_position(
            &app.composer.input,
            app.composer.cursor,
            inner.width as usize,
        );
        let visible_height = inner.height as usize;
        let start = cursor_y.saturating_add(1).saturating_sub(visible_height);
        let cursor_y = cursor_y.saturating_sub(start);
        let cx = inner.x + cursor_x.min(inner.width.saturating_sub(1) as usize) as u16;
        let cy = inner.y + cursor_y as u16;
        if inner.width > 0 && cy < inner.y + inner.height {
            f.set_cursor_position((cx, cy));
        }
    }
}

pub(crate) fn compute_cursor_position(input: &str, cursor: usize, width: usize) -> (usize, usize) {
    let before: Vec<char> = input.chars().take(cursor).collect();
    let mut x = 0usize;
    let mut y = 0usize;
    for (idx, ch) in before.iter().copied().enumerate() {
        if ch == '\n' {
            x = 0;
            y += 1;
        } else {
            let ch_width = char_display_width(ch);
            if width > 0 && ch_width > 0 && x + ch_width > width {
                x = 0;
                y += 1;
            }
            x += ch_width;
        }

        let next = before.get(idx + 1).copied();
        if width > 0 && x == width && next.is_some_and(|ch| ch != '\n') {
            x = 0;
            y += 1;
        }
    }
    if width > 0 && x == width && before.last().is_some_and(|ch| *ch != '\n') {
        x = 0;
        y += 1;
    }
    (x, y)
}

fn render_footer(f: &mut Frame, area: Rect, app: &App) {
    let status = app.status_line();
    let busy = app.is_busy();
    let shortcuts = if busy {
        "Ctrl+C:cancel  ↑↓:scroll  Shift+↑↓:history  Ctrl/Alt+4:agents  ?:help"
    } else {
        "Enter:send  Ctrl+C:quit  ↑↓:scroll  Shift+↑↓:history  Ctrl/Alt+4:agents  ?:help"
    };

    let available = area.width as usize;
    let status_width = UnicodeWidthStr::width(status.as_str());
    let sep = " │ ";
    let sep_width = UnicodeWidthStr::width(sep);
    let left_space = available.saturating_sub(status_width + sep_width + 2);

    let left_text = if UnicodeWidthStr::width(shortcuts) > left_space {
        truncate_display_width(shortcuts, left_space)
    } else {
        shortcuts.to_string()
    };

    let left_width = UnicodeWidthStr::width(left_text.as_str());
    let padding = available.saturating_sub(left_width + sep_width + status_width + 2);

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
                ("Enter", "Send message / queue while busy"),
                ("Alt+S", "Steer: inject queued message into running task"),
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
                ("↑ / ↓", "Scroll transcript"),
                ("Shift+↑ / Shift+↓", "Browse input history"),
                ("PgUp / PgDn", "Page scroll"),
                ("Mouse wheel", "Scroll transcript"),
            ],
        ),
        (
            "Agents & Commands",
            vec![
                ("Ctrl+4 / Alt+4", "Cycle: sidebar / agent overlay / hide"),
                ("/agents", "Toggle agents sidebar"),
                ("/help or ?", "Toggle this help"),
                ("/clear", "Clear & reset session"),
                ("/exit or /quit", "Exit xbot"),
                ("/stop", "Cancel current turn"),
                ("/new", "Start new session"),
                ("/session", "Switch / delete sessions"),
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
    use xbot::diff::DiffKind;

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
    if let Some(source) = &dialog.source {
        text_lines.push(Line::from(vec![
            Span::styled("  From: ", Style::default().fg(TEXT_DIM)),
            Span::styled(
                truncate_end(source, inner_w.saturating_sub(8)),
                Style::default().fg(TEXT_MUTED),
            ),
        ]));
    }
    text_lines.push(Line::from(""));

    let max_lines = height.saturating_sub(if dialog.source.is_some() { 9 } else { 8 }) as usize;
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

fn format_subagent_token_hint(usage: &SubagentTokenUsage) -> String {
    let cache_hint = if usage.cached_tokens > 0 && usage.prompt_tokens > 0 {
        let pct = (usage.cached_tokens * 100) / usage.prompt_tokens;
        format!("({}% cached) ", pct)
    } else {
        String::new()
    };
    format!(
        "↑{} {}↓{}",
        usage.prompt_tokens, cache_hint, usage.completion_tokens
    )
}

fn overlay_push_wrapped(
    lines: &mut Vec<Line<'static>>,
    text: &str,
    width: usize,
    style: Style,
    prefix: &str,
) {
    for src_line in text.lines() {
        let avail = width.saturating_sub(prefix.len());
        let wrapped = wrap_words(src_line, avail.max(1));
        for wl in wrapped {
            lines.push(Line::from(Span::styled(format!("{prefix}{wl}"), style)));
        }
    }
    if text.is_empty() {
        lines.push(Line::from(Span::styled(prefix.to_string(), style)));
    }
}

fn render_subagent_overlay(f: &mut Frame, area: Rect, app: &mut super::app::App) {
    let agents: Vec<_> = app.subagents.values().collect();
    if agents.is_empty() {
        app.show_subagent_overlay = false;
        return;
    }

    let idx = app.subagent_overlay_index.min(agents.len() - 1);
    app.subagent_overlay_index = idx;
    let agent = &agents[idx];

    let width = area.width.saturating_sub(4).max(40);
    let height = area.height.saturating_sub(4).max(10);
    let x = (area.width.saturating_sub(width)) / 2;
    let y = (area.height.saturating_sub(height)) / 2;
    let popup = Rect::new(x, y, width, height);

    f.render_widget(Clear, popup);

    let inner_w = width.saturating_sub(4) as usize;
    let text_w = inner_w.saturating_sub(2);
    let mut content_lines: Vec<Line<'static>> = Vec::new();

    let (status_str, status_color) = match &agent.status {
        SubagentStatus::Running => ("running", SUBAGENT_RUNNING),
        SubagentStatus::Completed => ("completed", SUBAGENT_DONE),
        SubagentStatus::Failed => ("failed", SUBAGENT_FAIL),
        SubagentStatus::Cancelled => ("cancelled", TEXT_DIM),
    };
    content_lines.push(Line::from(vec![
        Span::styled("  Status: ", Style::default().fg(TEXT_DIM)),
        Span::styled(
            status_str,
            Style::default()
                .fg(status_color)
                .add_modifier(Modifier::BOLD),
        ),
    ]));

    if !agent.model.is_empty() {
        content_lines.push(Line::from(vec![
            Span::styled("  Model:  ", Style::default().fg(TEXT_DIM)),
            Span::styled(agent.model.clone(), Style::default().fg(TEXT_MUTED)),
        ]));
    }

    let elapsed = agent
        .finished_at
        .map(|finished_at| finished_at.saturating_duration_since(agent.started_at))
        .unwrap_or_else(|| agent.started_at.elapsed());
    content_lines.push(Line::from(vec![
        Span::styled("  Time:   ", Style::default().fg(TEXT_DIM)),
        Span::styled(
            super::app::format_elapsed(elapsed),
            Style::default().fg(TEXT_MUTED),
        ),
    ]));

    content_lines.push(Line::from(""));

    content_lines.push(Line::from(Span::styled(
        "  Task",
        Style::default()
            .fg(ACCENT)
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
    )));
    overlay_push_wrapped(
        &mut content_lines,
        &agent.task,
        inner_w,
        Style::default().fg(TEXT_PRIMARY),
        "  ",
    );

    if !agent.all_actions.is_empty() {
        content_lines.push(Line::from(""));
        content_lines.push(Line::from(Span::styled(
            "  Tool Actions",
            Style::default()
                .fg(ACCENT)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        )));
        for action in &agent.all_actions {
            let action_w = text_w.saturating_sub(2);
            let wrapped = wrap_words(action, action_w.max(1));
            for (i, wl) in wrapped.iter().enumerate() {
                let bullet = if i == 0 { "  ▸ " } else { "    " };
                content_lines.push(Line::from(vec![
                    Span::styled(bullet, Style::default().fg(TOOL_FG)),
                    Span::styled(wl.clone(), Style::default().fg(TEXT_MUTED)),
                ]));
            }
        }
    }

    if !agent.reasoning_chunks.is_empty() {
        content_lines.push(Line::from(""));
        content_lines.push(Line::from(Span::styled(
            "  ▼ Thinking Process",
            Style::default().fg(TEXT_DIM).add_modifier(Modifier::BOLD),
        )));
        for chunk in &agent.reasoning_chunks {
            overlay_push_wrapped(
                &mut content_lines,
                chunk,
                inner_w,
                Style::default().fg(TEXT_DIM).add_modifier(Modifier::ITALIC),
                "    ",
            );
        }
    }

    if !agent.text_chunks.is_empty() {
        content_lines.push(Line::from(""));
        content_lines.push(Line::from(Span::styled(
            "  Response",
            Style::default()
                .fg(ACCENT)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        )));
        for chunk in &agent.text_chunks {
            let md_lines = markdown::markdown_to_lines(chunk, text_w);
            for ml in md_lines {
                let mut prefixed = vec![Span::raw("  ")];
                prefixed.extend(ml.spans);
                content_lines.push(Line::from(prefixed));
            }
        }
    }

    if let Some(result) = &agent.full_result {
        if agent.text_chunks.is_empty() {
            content_lines.push(Line::from(""));
            content_lines.push(Line::from(Span::styled(
                "  Result",
                Style::default()
                    .fg(ACCENT)
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            )));
            let md_lines = markdown::markdown_to_lines(result, text_w);
            for ml in md_lines {
                let mut prefixed = vec![Span::raw("  ")];
                prefixed.extend(ml.spans);
                content_lines.push(Line::from(prefixed));
            }
        }
    }

    let total_content_lines = content_lines.len();
    let visible_height = height.saturating_sub(4) as usize;
    let max_scroll = total_content_lines.saturating_sub(visible_height);
    app.subagent_overlay_scroll = app.subagent_overlay_scroll.min(max_scroll);

    let nav_left = if idx > 0 { "◀ " } else { "  " };
    let nav_right = if idx < agents.len() - 1 { " ▶" } else { "  " };
    let title = format!(
        "{nav_left}{} [{}/{}]{nav_right}",
        agent.label,
        idx + 1,
        agents.len()
    );
    let footer_str = " Esc/Ctrl+4:close  ←/→:switch  ↑/↓:scroll ";

    let block = Block::default()
        .title(Span::styled(
            format!(" {title} "),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ))
        .title_alignment(Alignment::Center)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACCENT))
        .style(Style::default().bg(HEADER_BG));

    let para = Paragraph::new(Text::from(content_lines))
        .block(block)
        .scroll((app.subagent_overlay_scroll as u16, 0));

    f.render_widget(para, popup);

    if total_content_lines > visible_height {
        let mut scrollbar_state = ScrollbarState::new(total_content_lines)
            .position(app.subagent_overlay_scroll)
            .viewport_content_length(visible_height);
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None);
        let scrollbar_area = Rect {
            x: popup.x + popup.width - 1,
            y: popup.y + 1,
            width: 1,
            height: popup.height.saturating_sub(2),
        };
        f.render_stateful_widget(scrollbar, scrollbar_area, &mut scrollbar_state);
    }

    let footer_line = Line::from(Span::styled(footer_str, Style::default().fg(TEXT_DIM)));
    let footer_x = popup.x + (popup.width.saturating_sub(footer_str.len() as u16)) / 2;
    let footer_y = popup.y + popup.height - 1;
    if footer_y < area.height && footer_x < area.width {
        let footer_area = Rect::new(
            footer_x,
            footer_y,
            (footer_str.len() as u16).min(area.width - footer_x),
            1,
        );
        f.render_widget(Paragraph::new(footer_line), footer_area);
    }
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

fn truncate_display_width(text: &str, max_width: usize) -> String {
    let mut out = String::new();
    let mut width = 0usize;
    for ch in text.chars() {
        let ch_width = char_display_width(ch);
        if width + ch_width > max_width {
            break;
        }
        out.push(ch);
        width += ch_width;
    }
    out
}

fn format_session_context_tokens(tokens: Option<usize>) -> String {
    match tokens {
        Some(tokens) if tokens >= 1000 => format!("{}k tok", tokens / 1000),
        Some(tokens) => format!("{tokens} tok"),
        None => "ctx n/a".to_string(),
    }
}

fn render_session_overlay(f: &mut Frame, area: Rect, app: &mut super::app::App) {
    if app.available_sessions.is_empty() {
        app.show_session_overlay = false;
        return;
    }

    let count = app.available_sessions.len();
    let list_height = (count as u16).min(12);
    let height = list_height + 4;
    let width = area.width.saturating_sub(8).min(72).max(40);
    let x = (area.width.saturating_sub(width)) / 2;
    let y = (area.height.saturating_sub(height)) / 2;
    let popup = Rect::new(x, y, width, height);

    f.render_widget(Clear, popup);

    let inner_w = width.saturating_sub(4) as usize;
    let mut lines: Vec<Line<'static>> = Vec::new();

    let idx = app.session_overlay_index.min(count.saturating_sub(1));
    app.session_overlay_index = idx;

    let pending_delete = app.session_delete_confirm;

    for (i, s) in app.available_sessions.iter().enumerate() {
        let is_selected = i == idx;
        let is_current = s.key == app.session_key;
        let is_delete_target = pending_delete == Some(i);
        let marker = if is_current { "● " } else { "  " };
        let prefix = if is_selected { "▸ " } else { "  " };

        let title = truncate_end(&s.title, inner_w.saturating_sub(30));
        let ago = format_relative_time_short(&s.updated_at);
        let tokens = format_session_context_tokens(s.context_tokens);
        let detail = format!("{} msgs · {} · {}", s.message_count, tokens, ago);

        let title_style = if is_delete_target {
            Style::default()
                .fg(Color::Rgb(255, 100, 100))
                .add_modifier(Modifier::BOLD)
        } else if is_selected {
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
        } else if is_current {
            Style::default().fg(Color::Rgb(160, 200, 240))
        } else {
            Style::default().fg(TEXT_MUTED)
        };

        let detail_style = if is_delete_target {
            Style::default().fg(Color::Rgb(200, 80, 80))
        } else if is_selected {
            Style::default().fg(Color::Rgb(140, 160, 180))
        } else {
            Style::default().fg(TEXT_DIM)
        };

        let marker_style = if is_delete_target {
            Style::default().fg(Color::Rgb(255, 100, 100))
        } else if is_current {
            Style::default().fg(ACCENT)
        } else {
            Style::default().fg(TEXT_DIM)
        };

        lines.push(Line::from(vec![
            Span::styled(format!("{marker}{prefix}"), marker_style),
            Span::styled(title, title_style),
            if is_delete_target {
                Span::styled(" ✗ delete?", Style::default().fg(Color::Rgb(255, 120, 120)))
            } else {
                Span::raw("")
            },
        ]));
        lines.push(Line::from(vec![
            Span::raw("      "),
            Span::styled(detail, detail_style),
        ]));
    }

    let block = Block::default()
        .title(Span::styled(
            format!(" Sessions [{count}] "),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ))
        .title_alignment(Alignment::Center)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACCENT))
        .style(Style::default().bg(HEADER_BG));

    let scroll_offset = {
        let visible = height.saturating_sub(4) as usize;
        let line_idx = idx * 2;
        if line_idx + 2 > app.session_overlay_scroll + visible {
            line_idx.saturating_sub(visible.saturating_sub(2))
        } else if line_idx < app.session_overlay_scroll {
            line_idx
        } else {
            app.session_overlay_scroll
        }
    };
    app.session_overlay_scroll = scroll_offset;

    let para = Paragraph::new(Text::from(lines))
        .block(block)
        .scroll((scroll_offset as u16, 0));

    f.render_widget(para, popup);

    let footer_spans = if app.session_delete_confirm.is_some() {
        vec![
            Span::styled(
                " Delete this session? ",
                Style::default().fg(Color::Rgb(255, 120, 120)),
            ),
            Span::styled(
                "y",
                Style::default()
                    .fg(Color::Rgb(255, 100, 100))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("es / ", Style::default().fg(TEXT_DIM)),
            Span::styled(
                "n",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled("o ", Style::default().fg(TEXT_DIM)),
        ]
    } else {
        let is_current = app
            .available_sessions
            .get(app.session_overlay_index)
            .is_some_and(|s| s.key == app.session_key);
        let mut spans = vec![
            Span::styled(" ↑↓", Style::default().fg(TEXT_MUTED)),
            Span::styled(":select  ", Style::default().fg(TEXT_DIM)),
            Span::styled("Enter", Style::default().fg(TEXT_MUTED)),
            Span::styled(":switch  ", Style::default().fg(TEXT_DIM)),
            Span::styled("d", Style::default().fg(Color::Rgb(255, 140, 140))),
            Span::styled(":delete  ", Style::default().fg(TEXT_DIM)),
        ];
        if is_current {
            spans.push(Span::styled(
                "c",
                Style::default().fg(Color::Rgb(255, 200, 100)),
            ));
            spans.push(Span::styled(":clear  ", Style::default().fg(TEXT_DIM)));
        }
        spans.push(Span::styled("Esc", Style::default().fg(TEXT_MUTED)));
        spans.push(Span::styled(":close ", Style::default().fg(TEXT_DIM)));
        spans
    };
    let footer_line = Line::from(footer_spans.clone());
    let footer_width: usize = footer_spans.iter().map(|s| s.content.len()).sum();
    let footer_x = popup.x + (popup.width.saturating_sub(footer_width as u16)) / 2;
    let footer_y = popup.y + popup.height - 1;
    if footer_y < area.height && footer_x < area.width {
        let footer_area = Rect::new(
            footer_x,
            footer_y,
            (footer_width as u16).min(area.width - footer_x),
            1,
        );
        f.render_widget(Paragraph::new(footer_line), footer_area);
    }
}

fn format_relative_time_short(iso: &str) -> String {
    use chrono::{DateTime, Utc};
    let Ok(dt) = iso.parse::<DateTime<Utc>>() else {
        return iso.to_string();
    };
    let dur = Utc::now().signed_duration_since(dt);
    if dur.num_seconds() < 60 {
        "just now".to_string()
    } else if dur.num_minutes() < 60 {
        format!("{}m ago", dur.num_minutes())
    } else if dur.num_hours() < 24 {
        format!("{}h ago", dur.num_hours())
    } else {
        format!("{}d ago", dur.num_days())
    }
}
