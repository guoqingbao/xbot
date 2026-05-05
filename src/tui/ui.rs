use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap,
};

use super::app::{ActiveStreaming, App, EditDiff, HistoryEntry, StreamSegment};
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
const SEPARATOR_FG: Color = Color::Rgb(55, 65, 85);
const COMPOSER_BG: Color = Color::Rgb(16, 20, 30);
const TRANSCRIPT_BG: Color = Color::Rgb(12, 14, 22);

pub fn render(f: &mut Frame, app: &mut App) {
    let area = f.area();

    let composer_height = composer_height(&app.composer.input);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(composer_height),
            Constraint::Length(1),
        ])
        .split(area);

    render_header(f, chunks[0], app);
    render_transcript(f, chunks[1], app);
    render_composer(f, chunks[2], app);
    render_footer(f, chunks[3], app);

    if app.show_help {
        render_help_overlay(f, area);
    }
}

fn composer_height(input: &str) -> u16 {
    let lines = input.matches('\n').count() + 1;
    (lines as u16 + 2).min(12).max(3)
}

fn render_header(f: &mut Frame, area: Rect, app: &App) {
    let (status_icon, status_color) = if app.is_busy {
        (format!("{} working", app.spinner_char()), WORKING_FG)
    } else {
        ("● ready".to_string(), SUCCESS_FG)
    };

    let sep = Span::styled(" │ ", Style::default().fg(BORDER_DIM));

    let mut spans = vec![
        Span::styled(
            " ■ rbot ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        sep.clone(),
        Span::styled(app.model.clone(), Style::default().fg(TEXT_PRIMARY)),
        sep.clone(),
        Span::styled(app.provider.clone(), Style::default().fg(TEXT_MUTED)),
        sep.clone(),
        Span::styled(
            format!("ctx:{}", app.context_status),
            Style::default().fg(TEXT_MUTED),
        ),
        sep,
        Span::styled(status_icon, Style::default().fg(status_color)),
    ];

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
        lines.extend(build_active_lines(active, inner_width, app.animation_frame));
    }

    let pending_preview = app.line_buffer.pending_preview();
    if !pending_preview.is_empty() && app.is_busy {
        let md = markdown::markdown_to_lines(pending_preview, inner_width.saturating_sub(2).max(1));
        for ml in md {
            let mut prefixed = vec![Span::raw("  ")];
            prefixed.extend(ml.spans);
            lines.push(Line::from(prefixed));
        }
    }

    if app.is_busy
        && app.active.as_ref().map_or(true, |a| !a.has_content())
        && pending_preview.is_empty()
    {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("  {} thinking…", app.spinner_char()),
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

fn build_transcript_lines(app: &App, width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let w = width.saturating_sub(4).max(1);

    for entry in &app.history {
        match entry {
            HistoryEntry::User(text) => {
                lines.push(Line::from(""));
                let display = if text.chars().count() > 200 {
                    let truncated: String = text.chars().take(200).collect();
                    format!("{truncated}…")
                } else {
                    text.clone()
                };
                let user_lines: Vec<&str> = display.lines().collect();
                for (i, uline) in user_lines.iter().enumerate() {
                    if i == 0 {
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
                lines.push(Line::from(""));
                let md_lines = markdown::markdown_to_lines(content, w);
                for ml in md_lines {
                    let mut prefixed = vec![Span::raw("  ")];
                    prefixed.extend(ml.spans);
                    lines.push(Line::from(prefixed));
                }
            }
            HistoryEntry::ToolCall {
                name,
                emoji,
                detail,
                diff,
            } => {
                lines.push(Line::from(""));
                render_tool_card(&mut lines, name, emoji, detail, diff.as_ref(), w, false, 0);
            }
            HistoryEntry::Error(msg) => {
                lines.push(Line::from(""));
                lines.push(Line::from(vec![
                    Span::styled(
                        "  ✗ ",
                        Style::default().fg(ERROR_FG).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(msg.clone(), Style::default().fg(ERROR_FG)),
                ]));
            }
            HistoryEntry::System(msg) => {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    format!("  {msg}"),
                    Style::default().fg(TEXT_DIM),
                )));
            }
            HistoryEntry::Separator { summary } => {
                render_turn_separator(&mut lines, summary, w);
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
    lines.push(Line::from(""));
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

fn build_active_lines(
    active: &ActiveStreaming,
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
                    lines.push(Line::from(""));
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
            StreamSegment::Tool(tool) => {
                lines.push(Line::from(""));
                render_tool_card(
                    &mut lines,
                    &tool.name,
                    &tool.emoji,
                    &tool.detail,
                    tool.diff.as_ref(),
                    w,
                    is_last,
                    anim_frame,
                );
            }
        }
    }

    lines
}

fn render_tool_card(
    lines: &mut Vec<Line<'static>>,
    name: &str,
    emoji: &str,
    detail: &str,
    diff: Option<&EditDiff>,
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

    let state_text = if running { "running" } else { "done" };
    let header_fill = "─".repeat(
        w.saturating_sub(10 + display_name.len() + state_text.len())
            .min(60),
    );

    lines.push(Line::from(vec![
        Span::styled(
            format!("  {status_sym} "),
            Style::default().fg(status_color),
        ),
        Span::styled(format!("{glyph} "), Style::default().fg(TOOL_FG)),
        Span::styled(
            display_name,
            Style::default()
                .fg(TEXT_PRIMARY)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" {state_text} "), Style::default().fg(TEXT_DIM)),
        Span::styled(header_fill, Style::default().fg(BORDER_DIM)),
    ]));

    if !detail.is_empty() {
        for dline in detail.lines() {
            lines.push(Line::from(vec![
                Span::styled("    │ ", Style::default().fg(BORDER_DIM)),
                Span::styled(
                    truncate_end(dline, w.saturating_sub(6)),
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

    if let Some(diff) = diff {
        render_edit_diff(lines, diff, w);
    }

    let bottom_fill = "─".repeat(w.saturating_sub(4).min(60));
    lines.push(Line::from(Span::styled(
        format!("    └{bottom_fill}"),
        Style::default().fg(BORDER_DIM),
    )));
}

fn render_edit_diff(lines: &mut Vec<Line<'static>>, diff: &EditDiff, w: usize) {
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

    let max_diff_lines = 12;
    let total_diff = diff.removals.len() + diff.additions.len();
    let (show_rem, show_add) = if total_diff <= max_diff_lines {
        (diff.removals.len(), diff.additions.len())
    } else {
        let ratio_rem = if total_diff > 0 {
            (diff.removals.len() * max_diff_lines) / total_diff
        } else {
            0
        };
        let rem = ratio_rem.max(1).min(diff.removals.len());
        let add = (max_diff_lines - rem).min(diff.additions.len());
        (rem, add)
    };

    for line in diff.removals.iter().take(show_rem) {
        lines.push(Line::from(vec![
            Span::styled("    │ ", Style::default().fg(BORDER_DIM)),
            Span::styled(
                format!("- {}", truncate_end(line, max_w.saturating_sub(2))),
                Style::default().fg(DIFF_DEL),
            ),
        ]));
    }
    if diff.removals.len() > show_rem {
        lines.push(Line::from(vec![
            Span::styled("    │ ", Style::default().fg(BORDER_DIM)),
            Span::styled(
                format!("  … {} more removed", diff.removals.len() - show_rem),
                Style::default().fg(TEXT_DIM),
            ),
        ]));
    }

    for line in diff.additions.iter().take(show_add) {
        lines.push(Line::from(vec![
            Span::styled("    │ ", Style::default().fg(BORDER_DIM)),
            Span::styled(
                format!("+ {}", truncate_end(line, max_w.saturating_sub(2))),
                Style::default().fg(DIFF_ADD),
            ),
        ]));
    }
    if diff.additions.len() > show_add {
        lines.push(Line::from(vec![
            Span::styled("    │ ", Style::default().fg(BORDER_DIM)),
            Span::styled(
                format!("  … {} more added", diff.additions.len() - show_add),
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
    let border_color = if app.is_busy { TEXT_DIM } else { ACCENT };
    let title = if app.is_busy {
        if app.pending.is_empty() {
            format!(" {} working… ", app.spinner_char())
        } else {
            format!(
                " {} working… ({} queued) ",
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
            Style::default().fg(if app.is_busy { TEXT_DIM } else { TEXT_MUTED }),
        ))
        .style(Style::default().bg(COMPOSER_BG));

    let inner = block.inner(area);

    let display_text = if app.composer.input.is_empty() && !app.is_busy {
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

    if !app.show_help && !app.is_busy {
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

fn compute_cursor_position(input: &str, cursor: usize, width: usize) -> (usize, usize) {
    let before: String = input.chars().take(cursor).collect();
    let mut x = 0usize;
    let mut y = 0usize;
    for ch in before.chars() {
        if ch == '\n' {
            x = 0;
            y += 1;
        } else {
            x += 1;
            if width > 0 && x >= width {
                x = 0;
                y += 1;
            }
        }
    }
    (x, y)
}

fn render_footer(f: &mut Frame, area: Rect, app: &App) {
    let status = app.status_line();
    let shortcuts = if app.is_busy {
        "Ctrl+C:cancel  ↑↓:history  Shift+↑↓:scroll  F1/?:help"
    } else {
        "Enter:send  Ctrl+C:quit  ↑↓:history  Shift+↑↓:scroll  F1/?:help"
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

    let status_color = if app.is_busy { WORKING_FG } else { ACCENT };

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
    let height = area.height.min(30);
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
            "Commands",
            vec![
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
        parts.push(format!("↑{} ↓{}", s.prompt_tokens, s.completion_tokens));
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
