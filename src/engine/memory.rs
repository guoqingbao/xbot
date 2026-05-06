use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::storage::{ChatMessage, Session};
use crate::util::{
    DEFAULT_HISTORY_TEMPLATE, DEFAULT_MEMORY_TEMPLATE, ensure_dir, estimate_json_tokens, now_iso,
    workspace_state_dir,
};

const MEMORY_ENTRIES_HEADING: &str = "## Memory Entries";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryEntryKind {
    TaskSummary,
    UserInstructed,
    ConsolidationSummary,
}

#[derive(Debug, Clone)]
pub struct ConsolidationResult {
    pub history_entry: String,
    pub memory_update: Option<String>,
    pub messages_consolidated: usize,
}

impl MemoryEntryKind {
    fn label(self) -> &'static str {
        match self {
            Self::TaskSummary => "Task Summary",
            Self::UserInstructed => "User Instructed Memory",
            Self::ConsolidationSummary => "Consolidation Summary",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryEntry {
    pub kind: MemoryEntryKind,
    pub title: String,
    pub summary: String,
    pub attention_points: Vec<String>,
    pub recorded_at: String,
}

impl MemoryEntry {
    pub fn to_markdown(&self) -> String {
        let mut lines = vec![
            format!("### {}", self.title.trim()),
            format!("- Type: {}", self.kind.label()),
            format!("- Time: {}", self.recorded_at.trim()),
            format!("- Summary: {}", self.summary.trim()),
        ];
        if self.attention_points.is_empty() {
            lines.push("- Attention: none recorded".to_string());
        } else {
            lines.push("- Attention:".to_string());
            for point in &self.attention_points {
                lines.push(format!("  - {}", point.trim()));
            }
        }
        lines.join("\n")
    }
}

pub struct MemoryStore {
    memory_dir: PathBuf,
    memory_file: PathBuf,
    history_file: PathBuf,
    max_memory_bytes: usize,
}

impl MemoryStore {
    pub fn new(workspace: &Path, max_memory_bytes: usize) -> Result<Self> {
        let memory_dir = workspace_state_dir(workspace).join("memory");
        Ok(Self {
            memory_file: memory_dir.join("MEMORY.md"),
            history_file: memory_dir.join("HISTORY.md"),
            memory_dir,
            max_memory_bytes: max_memory_bytes.max(1),
        })
    }

    pub fn read_long_term(&self) -> Result<String> {
        Ok(if self.memory_file.exists() {
            fs::read_to_string(&self.memory_file)?
        } else {
            DEFAULT_MEMORY_TEMPLATE.to_string()
        })
    }

    pub fn write_long_term(&self, content: &str) -> Result<()> {
        let (preface, mut entries) = split_memory_document(content);
        let rendered = render_memory_document(&preface, &mut entries, self.max_memory_bytes);
        ensure_dir(&self.memory_dir)?;
        fs::write(&self.memory_file, rendered)?;
        Ok(())
    }

    pub fn append_memory_entry(&self, entry: &MemoryEntry) -> Result<()> {
        let current = self.read_long_term()?;
        let (preface, mut entries) = split_memory_document(&current);
        entries.push(entry.to_markdown());
        let rendered = render_memory_document(&preface, &mut entries, self.max_memory_bytes);
        ensure_dir(&self.memory_dir)?;
        fs::write(&self.memory_file, rendered)?;
        Ok(())
    }

    pub fn append_history(&self, entry: &str) -> Result<()> {
        ensure_dir(&self.memory_dir)?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.history_file)?;
        writeln!(file, "{}\n", entry.trim_end())?;
        Ok(())
    }

    pub fn reset_history(&self) -> Result<()> {
        ensure_dir(&self.memory_dir)?;
        fs::write(&self.history_file, DEFAULT_HISTORY_TEMPLATE)?;
        Ok(())
    }

    pub fn get_memory_context(&self, topic: &str) -> Result<String> {
        let current = self.read_long_term()?;
        let (preface, entries) = split_memory_document(&current);
        let preface = extract_preface_context(&preface);
        let relevant_entries = select_relevant_entries(topic, &entries, self.max_memory_bytes / 4);

        let mut parts = Vec::new();
        if !preface.trim().is_empty() {
            parts.push(preface);
        }
        if !relevant_entries.is_empty() {
            parts.push(format!(
                "## Relevant Memory Entries\n\n{}",
                relevant_entries.join("\n\n")
            ));
        }

        Ok(if parts.is_empty() {
            String::new()
        } else {
            format!("## Long-term Memory\n{}", parts.join("\n\n"))
        })
    }

    pub fn archive_raw_messages(&self, messages: &[ChatMessage]) -> Result<()> {
        if messages.is_empty() {
            return Ok(());
        }
        let mut lines = Vec::new();
        lines.push(format!(
            "[{}] [RAW] {} messages",
            now_iso().chars().take(16).collect::<String>(),
            messages.len()
        ));
        for message in messages {
            if let Some(text) = message.content_as_text() {
                lines.push(format!(
                    "[{}] {}: {}",
                    message
                        .timestamp
                        .as_deref()
                        .unwrap_or("?")
                        .chars()
                        .take(16)
                        .collect::<String>(),
                    message.role.to_uppercase(),
                    text
                ));
            }
        }
        self.append_history(&lines.join("\n"))
    }

    pub fn memory_dir(&self) -> &Path {
        &self.memory_dir
    }
}

const MAX_CONSOLIDATION_FAILURES: u32 = 3;

pub struct MemoryConsolidator {
    store: MemoryStore,
    consecutive_failures: std::sync::Mutex<u32>,
}

impl MemoryConsolidator {
    pub fn new(
        workspace: &Path,
        _context_window_tokens: usize,
        max_memory_bytes: usize,
    ) -> Result<Self> {
        Ok(Self {
            store: MemoryStore::new(workspace, max_memory_bytes)?,
            consecutive_failures: std::sync::Mutex::new(0),
        })
    }

    pub fn store(&self) -> &MemoryStore {
        &self.store
    }

    pub fn estimate_session_prompt_tokens(&self, session: &Session) -> usize {
        session
            .get_history(0)
            .iter()
            .map(|message| {
                let mut total = 4;
                if let Some(content) = &message.content {
                    total += estimate_json_tokens(content);
                }
                if let Some(tool_calls) = &message.tool_calls {
                    total += tool_calls.iter().map(estimate_json_tokens).sum::<usize>();
                }
                total
            })
            .sum()
    }

    /// First exclusive index after `from` for one raw-archive chunk (same heuristic as token fallback).
    fn chunk_end_exclusive(session: &Session, from: usize) -> usize {
        let remaining = &session.messages[from..];
        let boundary = remaining
            .iter()
            .position(|message| message.role == "user" && from > 0)
            .unwrap_or(remaining.len().min(8))
            .max(1);
        (from + boundary).min(session.messages.len())
    }

    /// Returns the exclusive end index of the next chunk to consolidate, or [`None`] if the session
    /// is within the 75% token budget or there is nothing left to consolidate.
    pub fn pick_consolidation_boundary(
        &self,
        session: &Session,
        context_window_tokens: usize,
    ) -> Option<usize> {
        if context_window_tokens == 0 {
            return None;
        }
        let target = (context_window_tokens * 3) / 4;
        if self.estimate_session_prompt_tokens(session) <= target {
            return None;
        }
        if session.last_consolidated >= session.messages.len() {
            return None;
        }
        Some(Self::chunk_end_exclusive(
            session,
            session.last_consolidated,
        ))
    }

    /// Builds a prompt for an LLM to summarize a slice of chat messages as JSON
    /// (`history_entry`, `memory_update`).
    #[allow(clippy::unused_self)] // Instance method for symmetry with other consolidator APIs
    pub fn build_consolidation_prompt(&self, messages: &[ChatMessage]) -> String {
        let mut out = String::from(
            "You are consolidating an older segment of a chat session.\n\
\n\
Summarize the messages below into durable, compact memory.\n\
\n\
Respond with ONLY a single JSON object (no markdown code fences, no commentary) using this exact shape:\n\
{\"history_entry\":\"...\",\"memory_update\":\"...\"}\n\
\n\
Field rules:\n\
- \"history_entry\": one brief plain-text line suitable for a chronological HISTORY log (what was discussed or decided).\n\
- \"memory_update\": durable facts, preferences, or decisions to remember in MEMORY.md, or null if nothing should be persisted.\n\
\n\
Do not paste long transcripts into either field. Prefer facts over narration.\n\
\n\
Messages to summarize:\n\n",
        );
        for message in messages {
            let ts = message
                .timestamp
                .as_deref()
                .unwrap_or("?")
                .chars()
                .take(16)
                .collect::<String>();
            let body = message
                .content_as_text()
                .unwrap_or_else(|| "[non-text content]".to_string());
            out.push_str(&format!(
                "[{ts}] {}: {}\n",
                message.role.to_uppercase(),
                body
            ));
        }
        out
    }

    /// Applies LLM-produced consolidation: appends `history_entry` to HISTORY.md, optionally
    /// appends `memory_update` as a [`MemoryEntryKind::ConsolidationSummary`] to MEMORY.md, and
    /// advances [`Session::last_consolidated`] to `consolidate_until_exclusive`.
    pub fn consolidate_with_summary(
        &self,
        session: &mut Session,
        history_entry: &str,
        memory_update: Option<&str>,
        consolidate_until_exclusive: usize,
    ) -> Result<ConsolidationResult> {
        let start = session.last_consolidated.min(session.messages.len());
        let end = consolidate_until_exclusive.min(session.messages.len());
        if end < start {
            anyhow::bail!(
                "consolidate_until_exclusive ({end}) must be >= last_consolidated ({start})"
            );
        }
        let messages_consolidated = end - start;

        self.store.append_history(history_entry.trim_end())?;
        if let Some(text) = memory_update {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                let entry = MemoryEntry {
                    kind: MemoryEntryKind::ConsolidationSummary,
                    title: "Consolidation summary".to_string(),
                    summary: trimmed.to_string(),
                    attention_points: Vec::new(),
                    recorded_at: now_iso(),
                };
                self.store.append_memory_entry(&entry)?;
            }
        }
        session.last_consolidated = end;

        Ok(ConsolidationResult {
            history_entry: history_entry.to_string(),
            memory_update: memory_update.map(|s| s.to_string()),
            messages_consolidated,
        })
    }

    /// Fallback when LLM-based consolidation is unavailable: archives raw message text to HISTORY.md
    /// in chunks until the session is back under the 75% token threshold.
    pub fn maybe_consolidate_raw_archive_by_tokens(
        &self,
        session: &mut Session,
        context_window_tokens: usize,
    ) -> Result<()> {
        if context_window_tokens == 0 {
            return Ok(());
        }
        let target = (context_window_tokens * 3) / 4;
        while self.estimate_session_prompt_tokens(session) > target
            && session.last_consolidated < session.messages.len()
        {
            let end = Self::chunk_end_exclusive(session, session.last_consolidated);
            let chunk = &session.messages[session.last_consolidated..end];
            self.store.archive_raw_messages(chunk)?;
            session.last_consolidated = end;
        }
        Ok(())
    }

    /// Attempts LLM-driven consolidation first; falls back to raw archive after
    /// [`MAX_CONSOLIDATION_FAILURES`] consecutive failures.
    pub fn maybe_consolidate_by_tokens(
        &self,
        session: &mut Session,
        context_window_tokens: usize,
    ) -> Result<()> {
        self.maybe_consolidate_raw_archive_by_tokens(session, context_window_tokens)
    }

    /// LLM-driven consolidation: sends a chunk to the provider with a `save_memory` tool,
    /// parses the JSON response, and applies it. Falls back to raw archive after repeated failures.
    pub async fn maybe_consolidate_by_tokens_with_provider(
        &self,
        session: &mut Session,
        context_window_tokens: usize,
        provider: &dyn crate::providers::LlmProvider,
        model: &str,
    ) -> Result<()> {
        let failures = *self.consecutive_failures.lock().expect("failures lock");
        if failures >= MAX_CONSOLIDATION_FAILURES {
            return self.maybe_consolidate_raw_archive_by_tokens(session, context_window_tokens);
        }

        while let Some(end) = self.pick_consolidation_boundary(session, context_window_tokens) {
            let chunk = &session.messages[session.last_consolidated..end];
            let prompt = self.build_consolidation_prompt(chunk);

            match self.try_llm_consolidation(provider, model, &prompt).await {
                Ok((history_entry, memory_update)) => {
                    *self.consecutive_failures.lock().expect("failures lock") = 0;
                    self.consolidate_with_summary(
                        session,
                        &history_entry,
                        memory_update.as_deref(),
                        end,
                    )?;
                }
                Err(_) => {
                    let mut f = self.consecutive_failures.lock().expect("failures lock");
                    *f += 1;
                    if *f >= MAX_CONSOLIDATION_FAILURES {
                        drop(f);
                        return self.maybe_consolidate_raw_archive_by_tokens(
                            session,
                            context_window_tokens,
                        );
                    }
                    self.store.archive_raw_messages(chunk)?;
                    session.last_consolidated = end;
                }
            }
        }
        Ok(())
    }

    async fn try_llm_consolidation(
        &self,
        provider: &dyn crate::providers::LlmProvider,
        model: &str,
        prompt: &str,
    ) -> Result<(String, Option<String>)> {
        let messages = vec![ChatMessage::text("user", prompt)];
        let response = provider
            .chat(&messages, None, Some(model), Some(2048), None)
            .await?;
        let text = response
            .content
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("empty consolidation response"))?;
        parse_consolidation_json(text)
    }

    pub fn reset_failure_count(&self) {
        *self.consecutive_failures.lock().expect("failures lock") = 0;
    }

    /// Exposes the number of consecutive LLM consolidation failures (for tests and diagnostics).
    pub fn consecutive_consolidation_failures(&self) -> u32 {
        *self.consecutive_failures.lock().expect("failures lock")
    }

    pub fn archive_messages(&self, messages: &[ChatMessage]) -> Result<()> {
        self.store.archive_raw_messages(messages)
    }
}

/// Parses the JSON object returned by the consolidation LLM call.
pub fn parse_consolidation_json(text: &str) -> Result<(String, Option<String>)> {
    let cleaned = text
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    let value: serde_json::Value = serde_json::from_str(cleaned)
        .map_err(|e| anyhow::anyhow!("invalid consolidation JSON: {e}"))?;
    let history_entry = value
        .get("history_entry")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing history_entry in consolidation response"))?
        .to_string();
    let memory_update = value
        .get("memory_update")
        .and_then(|v| if v.is_null() { None } else { v.as_str() })
        .map(|s| s.to_string());
    Ok((history_entry, memory_update))
}

fn split_memory_document(content: &str) -> (String, Vec<String>) {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return (default_memory_preface(), Vec::new());
    }

    let Some((preface, rest)) = trimmed.split_once(MEMORY_ENTRIES_HEADING) else {
        return (trimmed.to_string(), Vec::new());
    };

    let entries = rest
        .split("\n### ")
        .filter_map(|block| {
            let block = block.trim();
            if block.is_empty() {
                return None;
            }
            Some(if block.starts_with("### ") {
                block.to_string()
            } else {
                format!("### {block}")
            })
        })
        .collect();
    (preface.trim_end().to_string(), entries)
}

fn render_memory_document(
    preface: &str,
    entries: &mut Vec<String>,
    max_memory_bytes: usize,
) -> String {
    let preface = if preface.trim().is_empty() {
        default_memory_preface()
    } else {
        preface.trim_end().to_string()
    };

    loop {
        let mut rendered = format!("{preface}\n\n{MEMORY_ENTRIES_HEADING}");
        if !entries.is_empty() {
            rendered.push_str("\n\n");
            rendered.push_str(&entries.join("\n\n"));
        }
        rendered.push('\n');

        if rendered.len() <= max_memory_bytes || entries.is_empty() {
            if rendered.len() <= max_memory_bytes {
                return rendered;
            }
            return trim_to_last_bytes(&rendered, max_memory_bytes);
        }

        entries.remove(0);
    }
}

fn default_memory_preface() -> String {
    DEFAULT_MEMORY_TEMPLATE
        .split_once(MEMORY_ENTRIES_HEADING)
        .map(|(preface, _)| preface.trim_end().to_string())
        .unwrap_or_else(|| DEFAULT_MEMORY_TEMPLATE.trim_end().to_string())
}

fn trim_to_last_bytes(content: &str, max_bytes: usize) -> String {
    if content.len() <= max_bytes {
        return content.to_string();
    }

    let mut start = content.len().saturating_sub(max_bytes);
    while !content.is_char_boundary(start) && start < content.len() {
        start += 1;
    }
    content[start..].to_string()
}

fn extract_preface_context(preface: &str) -> String {
    let mut lines = Vec::new();
    for line in preface.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if is_default_memory_template_line(trimmed) {
            continue;
        }
        if trimmed.starts_with("- ") && trimmed.ends_with(':') {
            continue;
        }
        lines.push(trimmed.to_string());
    }
    lines.join("\n")
}

fn is_default_memory_template_line(line: &str) -> bool {
    matches!(
        line,
        "# Long-Term Memory"
            | "This file is the agent's permanent memory. Keep it concise, current, and durable."
            | "## What Belongs Here"
            | "- Stable project architecture facts"
            | "- Repository conventions and workflows"
            | "- User preferences that affect future work"
            | "- Important decisions that should survive conversation resets"
            | "- Structured task summaries worth recalling later"
            | "## What Does Not Belong Here"
            | "- Full chat transcripts"
            | "- Temporary debugging notes"
            | "- Large logs or raw command output"
            | "## Suggested Sections"
            | "### Project"
            | "### Conventions"
            | "### User"
            | "## Memory Entries"
            | "Add durable entries below. Keep the newest relevant entries near the end."
    )
}

fn select_relevant_entries(topic: &str, entries: &[String], max_bytes: usize) -> Vec<String> {
    if entries.is_empty() || max_bytes == 0 {
        return Vec::new();
    }

    let topic_terms = topic_terms(topic);
    let mut scored = entries
        .iter()
        .enumerate()
        .map(|(idx, entry)| {
            let lowered = entry.to_ascii_lowercase();
            let mut score = topic_terms
                .iter()
                .filter(|term| lowered.contains(term.as_str()))
                .count();
            if lowered.contains("user instructed memory") {
                score += 1;
            }
            (idx, score, entry.clone())
        })
        .collect::<Vec<_>>();

    scored.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| right.0.cmp(&left.0)));

    let mut selected = Vec::new();
    let mut used = 0;
    for (_, score, entry) in scored {
        if !selected.is_empty() && score == 0 {
            break;
        }
        let entry_len = entry.len();
        if used + entry_len > max_bytes && !selected.is_empty() {
            break;
        }
        used += entry_len;
        selected.push(entry);
        if selected.len() >= 4 {
            break;
        }
    }

    if selected.is_empty() {
        for entry in entries.iter().rev().take(2).rev() {
            let entry_len = entry.len();
            if used + entry_len > max_bytes && !selected.is_empty() {
                break;
            }
            used += entry_len;
            selected.push(entry.clone());
        }
    }

    selected
}

fn topic_terms(topic: &str) -> Vec<String> {
    topic
        .split(|ch: char| !ch.is_alphanumeric())
        .map(|part| part.trim().to_ascii_lowercase())
        .filter(|part| part.len() > 2)
        .filter(|part| {
            !matches!(
                part.as_str(),
                "the"
                    | "and"
                    | "for"
                    | "with"
                    | "from"
                    | "that"
                    | "this"
                    | "into"
                    | "when"
                    | "need"
                    | "have"
                    | "about"
                    | "session"
                    | "task"
                    | "clear"
                    | "memorize"
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{
        ConsolidationResult, MemoryConsolidator, MemoryEntry, MemoryEntryKind, MemoryStore,
    };
    use crate::storage::{ChatMessage, Session};
    use tempfile::tempdir;

    #[test]
    fn memory_store_trims_to_latest_entries_with_limit() {
        let dir = tempdir().unwrap();
        let store = MemoryStore::new(dir.path(), 2_000).unwrap();
        for idx in 0..8 {
            store
                .append_memory_entry(&MemoryEntry {
                    kind: MemoryEntryKind::TaskSummary,
                    title: format!("Entry {idx}"),
                    summary: format!("Summary {idx} {}", "x".repeat(80)),
                    attention_points: vec![format!("Point {idx}")],
                    recorded_at: "2026-03-24T00:00:00Z".to_string(),
                })
                .unwrap();
        }

        let content = store.read_long_term().unwrap();
        assert!(content.len() <= 2_000);
        assert!(content.contains("Entry 7"));
        assert!(!content.contains("Entry 0"));
    }

    #[test]
    fn memory_context_prefers_relevant_entries() {
        let dir = tempdir().unwrap();
        let store = MemoryStore::new(dir.path(), 32 * 1024).unwrap();
        store
            .append_memory_entry(&MemoryEntry {
                kind: MemoryEntryKind::TaskSummary,
                title: "Slack thread handling".to_string(),
                summary: "Thread replies must preserve metadata.".to_string(),
                attention_points: vec!["Check thread_ts".to_string()],
                recorded_at: "2026-03-24T00:00:00Z".to_string(),
            })
            .unwrap();
        store
            .append_memory_entry(&MemoryEntry {
                kind: MemoryEntryKind::TaskSummary,
                title: "Cron cleanup".to_string(),
                summary: "Cron state should be refreshed safely.".to_string(),
                attention_points: vec!["Avoid stale timers".to_string()],
                recorded_at: "2026-03-24T00:00:00Z".to_string(),
            })
            .unwrap();

        let context = store
            .get_memory_context("fix slack thread stop behavior")
            .unwrap();
        assert!(context.contains("Slack thread handling"));
        assert!(!context.contains("Cron cleanup"));
    }

    #[test]
    fn reset_history_restores_template() {
        let dir = tempdir().unwrap();
        let store = MemoryStore::new(dir.path(), 32 * 1024).unwrap();
        store.append_history("junk").unwrap();
        store.reset_history().unwrap();

        let history = std::fs::read_to_string(store.memory_dir().join("HISTORY.md")).unwrap();
        assert_eq!(history, crate::util::DEFAULT_HISTORY_TEMPLATE);
    }

    #[test]
    fn pick_consolidation_boundary_none_when_under_budget() {
        let dir = tempdir().unwrap();
        let c = MemoryConsolidator::new(dir.path(), 10_000, 32 * 1024).unwrap();
        let mut session = Session::new("t");
        session.add_message("user", "hello");
        assert!(c.pick_consolidation_boundary(&session, 10_000).is_none());
    }

    #[test]
    fn pick_consolidation_boundary_some_when_over_budget() {
        let dir = tempdir().unwrap();
        let c = MemoryConsolidator::new(dir.path(), 500, 32 * 1024).unwrap();
        let mut session = Session::new("t");
        let filler = "word ".repeat(200);
        for _ in 0..40 {
            session.add_message("user", &filler);
        }
        let end = c
            .pick_consolidation_boundary(&session, 500)
            .expect("expected consolidation");
        assert!(end > session.last_consolidated);
        assert!(end <= session.messages.len());
    }

    #[test]
    fn build_consolidation_prompt_includes_json_shape() {
        let dir = tempdir().unwrap();
        let c = MemoryConsolidator::new(dir.path(), 10_000, 32 * 1024).unwrap();
        let messages = vec![ChatMessage::text("user", "ping")];
        let p = c.build_consolidation_prompt(&messages);
        assert!(p.contains("history_entry"));
        assert!(p.contains("memory_update"));
        assert!(p.contains("ping"));
    }

    #[test]
    fn consolidate_with_summary_updates_files_and_session() {
        let dir = tempdir().unwrap();
        let c = MemoryConsolidator::new(dir.path(), 10_000, 32 * 1024).unwrap();
        let mut session = Session::new("t");
        session.add_message("user", "a");
        session.add_message("assistant", "b");
        session.add_message("user", "c");

        let r: ConsolidationResult = c
            .consolidate_with_summary(
                &mut session,
                "Worked on feature X.",
                Some("User prefers tabs."),
                2,
            )
            .unwrap();

        assert_eq!(r.history_entry, "Worked on feature X.");
        assert_eq!(r.memory_update.as_deref(), Some("User prefers tabs."));
        assert_eq!(r.messages_consolidated, 2);
        assert_eq!(session.last_consolidated, 2);

        let history = std::fs::read_to_string(c.store().memory_dir().join("HISTORY.md")).unwrap();
        assert!(history.contains("Worked on feature X."));

        let memory = c.store().read_long_term().unwrap();
        assert!(memory.contains("Consolidation summary"));
        assert!(memory.contains("User prefers tabs."));
        assert!(memory.contains("Consolidation Summary"));
    }

    #[test]
    fn consolidate_with_summary_skips_empty_memory_update_string() {
        let dir = tempdir().unwrap();
        let c = MemoryConsolidator::new(dir.path(), 10_000, 32 * 1024).unwrap();
        let mut session = Session::new("t");
        session.add_message("user", "x");

        let before = c.store().read_long_term().unwrap();
        c.consolidate_with_summary(&mut session, "Log line only.", Some("   "), 1)
            .unwrap();
        assert_eq!(c.store().read_long_term().unwrap(), before);
    }

    #[test]
    fn maybe_consolidate_raw_archive_by_tokens_archives_raw_chunks() {
        let dir = tempdir().unwrap();
        let c = MemoryConsolidator::new(dir.path(), 400, 32 * 1024).unwrap();
        let mut session = Session::new("t");
        let filler = "tok ".repeat(120);
        for i in 0..12 {
            session.add_message(if i % 2 == 0 { "user" } else { "assistant" }, &filler);
        }

        c.maybe_consolidate_raw_archive_by_tokens(&mut session, 400)
            .unwrap();

        assert!(session.last_consolidated > 0);
        let history = std::fs::read_to_string(c.store().memory_dir().join("HISTORY.md")).unwrap();
        assert!(history.contains("[RAW]"));
        assert!(
            c.estimate_session_prompt_tokens(&session) <= (400 * 3) / 4
                || session.last_consolidated >= session.messages.len()
        );
    }

    #[test]
    fn memory_entry_consolidation_summary_markdown() {
        let e = MemoryEntry {
            kind: MemoryEntryKind::ConsolidationSummary,
            title: "t".to_string(),
            summary: "s".to_string(),
            attention_points: vec![],
            recorded_at: "2026-01-01T00:00:00Z".to_string(),
        };
        assert!(e.to_markdown().contains("Consolidation Summary"));
    }
}
