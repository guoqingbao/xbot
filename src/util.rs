use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use base64::Engine;
use chrono::Local;
use serde_json::{Value, json};

pub const DEFAULT_MEMORY_TEMPLATE: &str = r#"# Long-Term Memory

This file is the agent's permanent memory. Keep it concise, current, and durable.

## What Belongs Here

- Stable project architecture facts
- Repository conventions and workflows
- User preferences that affect future work
- Important decisions that should survive conversation resets
- Structured task summaries worth recalling later

## What Does Not Belong Here

- Full chat transcripts
- Temporary debugging notes
- Large logs or raw command output

## Suggested Sections

### Project

- Purpose:
- Important directories:
- Build/test commands:

### Conventions

- Code style:
- Review expectations:
- Release rules:

### User

- Preferences:
- Communication style:
- Important standing requests:

## Memory Entries

Add durable entries below. Keep the newest relevant entries near the end.
"#;

pub const DEFAULT_HISTORY_TEMPLATE: &str = r#"# History Log

Append-only event log for conversation and memory consolidation.

- Search this file when you need to recall past events.
- Do not rely on this file as active context.
- Promote durable facts from here into `MEMORY.md` when they matter long term.

"#;

pub fn ensure_dir(path: impl AsRef<Path>) -> Result<PathBuf> {
    let path = path.as_ref();
    fs::create_dir_all(path)?;
    Ok(path.to_path_buf())
}

pub fn workspace_state_dir(workspace: &Path) -> PathBuf {
    workspace.join(".rbot")
}

pub fn detect_image_mime(data: &[u8]) -> Option<&'static str> {
    if data.len() >= 8 && &data[..8] == b"\x89PNG\r\n\x1a\n" {
        return Some("image/png");
    }
    if data.len() >= 3 && &data[..3] == b"\xff\xd8\xff" {
        return Some("image/jpeg");
    }
    if data.len() >= 6 && (&data[..6] == b"GIF87a" || &data[..6] == b"GIF89a") {
        return Some("image/gif");
    }
    if data.len() >= 12 && &data[..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    None
}

pub fn build_image_content_blocks(raw: &[u8], mime: &str, path: &str, label: &str) -> Vec<Value> {
    let encoded = base64::engine::general_purpose::STANDARD.encode(raw);
    vec![
        json!({
            "type": "image_url",
            "image_url": {"url": format!("data:{mime};base64,{encoded}")},
            "_meta": {"path": path},
        }),
        json!({"type": "text", "text": label}),
    ]
}

pub fn current_time_str() -> String {
    Local::now().format("%Y-%m-%d %H:%M (%A) (%Z)").to_string()
}

pub fn now_iso() -> String {
    Local::now().to_rfc3339()
}

pub fn safe_filename(name: &str) -> String {
    name.chars()
        .map(|ch| match ch {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            _ => ch,
        })
        .collect::<String>()
        .trim()
        .to_string()
}

pub fn split_message(content: &str, max_len: usize) -> Vec<String> {
    if content.is_empty() {
        return Vec::new();
    }
    if content.chars().count() <= max_len {
        return vec![content.to_string()];
    }
    let mut remaining = content.trim().to_string();
    let mut chunks = Vec::new();
    while !remaining.is_empty() {
        if remaining.chars().count() <= max_len {
            chunks.push(remaining);
            break;
        }
        let cut = remaining
            .char_indices()
            .nth(max_len)
            .map(|(idx, _)| idx)
            .unwrap_or(remaining.len());
        let candidate = &remaining[..cut];
        let split_at = candidate
            .rfind('\n')
            .or_else(|| candidate.rfind(' '))
            .filter(|pos| *pos > 0)
            .unwrap_or(cut);
        chunks.push(remaining[..split_at].to_string());
        remaining = remaining[split_at..].trim_start().to_string();
    }
    chunks
}

pub fn estimate_text_tokens(text: &str) -> usize {
    let chars = text.chars().count();
    (chars / 4).max(1)
}

pub fn estimate_json_tokens(value: &Value) -> usize {
    estimate_text_tokens(&value.to_string())
}

pub fn tool_emoji(tool_name: &str) -> &'static str {
    match tool_name {
        "read_file" => "📖",
        "write_file" => "💾",
        "edit_file" => "✍",
        "list_dir" => "📁",
        "exec" => "💻",
        "web_search" => "🔍",
        "web_fetch" => "🌐",
        "spawn" => "🤖",
        "cron" => "⏰",
        _ => "🛠",
    }
}

pub fn build_status_content(
    version: &str,
    model: &str,
    workspace: &str,
    uptime_seconds: u64,
    last_prompt_tokens: usize,
    last_completion_tokens: usize,
    context_window_tokens: usize,
    session_message_count: usize,
    context_tokens_estimate: usize,
) -> String {
    let uptime = if uptime_seconds >= 3600 {
        format!(
            "{}h {}m",
            uptime_seconds / 3600,
            (uptime_seconds % 3600) / 60
        )
    } else {
        format!("{}m {}s", uptime_seconds / 60, uptime_seconds % 60)
    };
    let pct = if context_window_tokens > 0 {
        (context_tokens_estimate * 100) / context_window_tokens
    } else {
        0
    };
    let total_tokens = last_prompt_tokens.saturating_add(last_completion_tokens);
    format!(
        "Model: {model}\nWorkspace: {workspace}\nUptime: {uptime}\nSession messages: {session_message_count}\nToken usage (last turn): {last_prompt_tokens} prompt + {last_completion_tokens} completion (total {total_tokens})\nContext window: {context_window_tokens} tokens\nContext: {context_tokens_estimate}/{context_window_tokens} ({pct}%)\nrbot v{version}"
    )
}

pub fn sync_workspace_templates(workspace: &Path) -> Result<Vec<PathBuf>> {
    sync_workspace_templates_with_memory(workspace, true)
}

pub fn sync_workspace_templates_without_memory(workspace: &Path) -> Result<Vec<PathBuf>> {
    sync_workspace_templates_with_memory(workspace, false)
}

fn sync_workspace_templates_with_memory(
    workspace: &Path,
    include_memory: bool,
) -> Result<Vec<PathBuf>> {
    let state_dir = ensure_dir(workspace_state_dir(workspace))?;
    migrate_legacy_workspace_state(workspace, &state_dir, include_memory)?;
    let mut created = Vec::new();
    let files = [
        (
            state_dir.join("AGENTS.md"),
            r#"# Agent Instructions

Use this file to describe repository-specific engineering rules and working agreements.

## Default Workflow

1. Inspect the relevant files before editing them.
2. Prefer small, verifiable changes over speculative rewrites.
3. Run focused checks after every meaningful code change.
4. Summarize what changed, what was verified, and any remaining risk.

## Memory Discipline

- Record durable facts in `memory/MEMORY.md` as soon as they become important.
- Durable facts include architecture decisions, repository conventions, environment setup, user preferences, and recurring project rules.
- Do not treat `memory/HISTORY.md` as active memory. It is an append-only log for later search.

## Scheduled and Long-Running Work

- Use cron-backed automation for recurring work instead of leaving ad hoc reminders in conversation history.
- Keep `HEARTBEAT.md` up to date when the workspace uses periodic review or background maintenance.

## Repository Notes

- Fill in language-specific build, test, lint, formatting, and deployment commands here.
- Add project rules such as branch strategy, review policy, and release steps.
"#,
        ),
        (
            state_dir.join("SOUL.md"),
            r#"# Soul

This file defines the workspace-specific personality and behavioral boundaries for `rbot`.

## Default Style

- Clear, direct, and technically precise
- Calm under ambiguity
- Honest about uncertainty and verification gaps

## Guardrails

- Accuracy over speed
- No destructive changes without explicit approval
- Prefer evidence, code reading, and verification over guesswork
- Keep user-facing communication concise and useful

## Optional Overrides

- Preferred tone for this repository
- Team-specific communication norms
- Extra safety constraints for production systems
"#,
        ),
        (
            state_dir.join("USER.md"),
            r#"# User Profile

Store durable user preferences and working agreements here.

## Communication

- Preferred tone:
- Preferred response length:
- Preferred level of technical detail:

## Work Context

- Primary role:
- Active projects:
- Typical stack:

## Long-Term Preferences

- Formatting preferences:
- Review expectations:
- Autonomy level:

## Stable Facts Worth Remembering

- Timezone:
- Language preferences:
- Important recurring constraints:
"#,
        ),
        (
            state_dir.join("TOOLS.md"),
            r#"# Tool Usage Notes

Document project-specific commands, wrappers, and operational caveats here.

## Build / Test / Lint

- Build:
- Test:
- Lint:
- Format:

## Runtime / Services

- Local server startup:
- Required environment variables:
- External dependencies:

## Safety Notes

- Commands that should never be run automatically:
- Slow or expensive commands:
- Commands that require credentials or VPN:
"#,
        ),
        (
            state_dir.join("memory").join("MEMORY.md"),
            DEFAULT_MEMORY_TEMPLATE,
        ),
        (
            state_dir.join("memory").join("HISTORY.md"),
            DEFAULT_HISTORY_TEMPLATE,
        ),
        (
            state_dir
                .join("skills")
                .join("memory-hygiene")
                .join("SKILL.md"),
            r#"---
description: "Always-on memory discipline for preserving durable workspace context."
metadata: {"rbot":{"always":true,"triggers":["remember","memory","preference","project context","decision"]}}
---

# Memory Hygiene

Use this guidance in every workspace.

## Two-Layer Memory

- `memory/MEMORY.md` is active long-term context. Keep it short, curated, and durable.
- `memory/HISTORY.md` is an append-only log for later search. It is not active context.

## Update MEMORY.md When

- the user states a stable preference
- you discover a durable repository rule or architecture fact
- a decision will matter in future sessions
- a recurring workflow or operational constraint becomes clear

## Memory Update Pattern

1. Check whether the fact is durable rather than temporary.
2. Write it into the right section of `memory/MEMORY.md`.
3. Keep entries compact and easy to scan.
4. Avoid dumping raw logs or transcript fragments into long-term memory.
"#,
        ),
        (
            state_dir
                .join("skills")
                .join("project-context")
                .join("SKILL.md"),
            r#"---
description: "Workspace template for project-specific architecture, commands, and conventions."
metadata: {"rbot":{"triggers":["project context","repo rules","architecture","workspace notes"]}}
---

# Project Context Template

Fill this skill with repository-specific details that should be easy for the agent to load.

## Suggested Content

- architecture overview
- key directories and ownership
- build/test/lint commands
- deployment or release flow
- integration caveats

## Guidance

- Prefer concise bullets over large prose blocks.
- Keep this file updated when the project structure changes.
- Mirror durable facts into `memory/MEMORY.md` when they should remain active all the time.
"#,
        ),
        (
            state_dir
                .join("skills")
                .join("delivery-rules")
                .join("SKILL.md"),
            r#"---
description: "Workspace template for review, release, and delivery expectations."
metadata: {"rbot":{"triggers":["review","release","deploy","handoff","delivery"]}}
---

# Delivery Rules Template

Use this file to document how work should be delivered in this project.

## Suggested Content

- review expectations
- test coverage standards
- release checklist
- commit or branch conventions
- deployment safeguards

## Guidance

- Keep this focused on process, not architecture.
- Update it when the team changes delivery policy.
- If a rule becomes universal for the workspace, also capture it in `memory/MEMORY.md`.
"#,
        ),
        (
            state_dir
                .join("skills")
                .join("memory-entry-writer")
                .join("SKILL.md"),
            r#"---
description: "Summarize durable memory entries for MEMORY.md after task completion or explicit memorize requests."
metadata: {"rbot":{"triggers":["memory entry","task summary","memorize","durable memory"]}}
---

# Memory Entry Writer

Use this skill when converting raw task output or user-provided durable facts into a compact `memory/MEMORY.md` entry.

## Output Shape

Return JSON only:

```json
{"title":"...","summary":"...","attention_points":["..."]}
```

## Rules

- `title`: plain text, short, specific, no markdown, under 80 characters
- `summary`: plain text, 1-2 short sentences, under 240 characters
- `attention_points`: short durable cautions, follow-ups, or constraints; use `[]` when empty
- Keep only durable facts worth remembering across sessions
- Do not copy code blocks, long quotes, raw logs, transcript filler, or markdown headings
- Prefer the repository, workflow, bug, decision, or user preference that will matter later
"#,
        ),
        (
            state_dir.join("HEARTBEAT.md"),
            r#"# Heartbeat

This file defines periodic tasks that rbot checks on a regular interval (default: every 30 minutes).

If there are no active tasks below, the heartbeat check is skipped.

## Active Tasks

<!-- Add recurring maintenance tasks here. Each task should be a clear, actionable item. -->
<!-- Example: -->
<!-- - Check for stale pull requests older than 3 days and post a reminder -->
<!-- - Review error logs and summarize new patterns -->

## Completed Tasks

<!-- Move completed tasks here with a timestamp for reference. -->
"#,
        ),
    ];
    ensure_dir(workspace)?;
    ensure_dir(state_dir.join("skills"))?;
    if include_memory {
        ensure_dir(state_dir.join("memory"))?;
    }
    for (path, content) in files {
        if !include_memory && is_memory_template_path(&path, &state_dir) {
            continue;
        }
        if let Some(parent) = path.parent() {
            ensure_dir(parent)?;
        }
        if !path.exists() {
            fs::write(&path, content)?;
            created.push(path);
        }
    }
    Ok(created)
}

fn is_memory_template_path(path: &Path, state_dir: &Path) -> bool {
    path.starts_with(state_dir.join("memory"))
        || path.starts_with(state_dir.join("skills").join("memory-hygiene"))
        || path.starts_with(state_dir.join("skills").join("memory-entry-writer"))
}

fn migrate_legacy_workspace_state(
    workspace: &Path,
    state_dir: &Path,
    include_memory: bool,
) -> Result<()> {
    for name in [
        "AGENTS.md",
        "SOUL.md",
        "USER.md",
        "TOOLS.md",
        "HEARTBEAT.md",
    ] {
        maybe_move_legacy_path(&workspace.join(name), &state_dir.join(name))?;
    }
    let state_dirs: &[&str] = if include_memory {
        &["memory", "sessions", "cron"]
    } else {
        &["sessions", "cron"]
    };
    for name in state_dirs.iter().copied() {
        maybe_move_legacy_path(&workspace.join(name), &state_dir.join(name))?;
    }
    Ok(())
}

fn maybe_move_legacy_path(source: &Path, target: &Path) -> Result<()> {
    if !source.exists() || target.exists() {
        return Ok(());
    }
    if let Some(parent) = target.parent() {
        ensure_dir(parent)?;
    }
    fs::rename(source, target)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{sync_workspace_templates, workspace_state_dir};
    use tempfile::tempdir;

    #[test]
    fn sync_workspace_templates_creates_memory_and_starter_skills() {
        let dir = tempdir().unwrap();
        let workspace = dir.path().join("workspace");

        let created = sync_workspace_templates(&workspace).unwrap();
        let state_dir = workspace_state_dir(&workspace);

        assert!(state_dir.join("AGENTS.md").is_file());
        assert!(state_dir.join("memory/MEMORY.md").is_file());
        assert!(state_dir.join("memory/HISTORY.md").is_file());
        assert!(state_dir.join("skills/memory-hygiene/SKILL.md").is_file());
        assert!(state_dir.join("skills/project-context/SKILL.md").is_file());
        assert!(state_dir.join("skills/delivery-rules/SKILL.md").is_file());
        assert!(
            state_dir
                .join("skills/memory-entry-writer/SKILL.md")
                .is_file()
        );
        assert!(
            created
                .iter()
                .any(|path| path.ends_with(".rbot/skills/memory-hygiene/SKILL.md"))
        );
    }
}
