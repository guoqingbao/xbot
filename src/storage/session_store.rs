use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::util::{ensure_dir, now_iso, safe_filename, workspace_state_dir};

pub const SESSION_CONTEXT_TOKENS_KEY: &str = "contextTokens";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChatMessage {
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_blocks: Option<Vec<Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<BTreeMap<String, Value>>,
}

impl ChatMessage {
    pub fn text(role: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            content: Some(Value::String(content.into())),
            tool_calls: None,
            tool_call_id: None,
            name: None,
            timestamp: Some(now_iso()),
            reasoning_content: None,
            thinking_blocks: None,
            metadata: None,
        }
    }

    pub fn content_as_text(&self) -> Option<String> {
        match &self.content {
            Some(Value::String(text)) => Some(text.clone()),
            Some(Value::Array(blocks)) => {
                let text = blocks
                    .iter()
                    .filter_map(|block| block.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n");
                (!text.is_empty()).then_some(text)
            }
            Some(other) => Some(other.to_string()),
            None => None,
        }
    }

    pub fn to_openai_payload(&self) -> Value {
        let mut out = json!({
            "role": self.role,
        });
        if let Some(content) = &self.content {
            match content {
                Value::Array(blocks) => {
                    let cleaned = blocks
                        .iter()
                        .map(|block| {
                            let mut b = block.clone();
                            if let Some(obj) = b.as_object_mut() {
                                obj.remove("_meta");
                            }
                            b
                        })
                        .collect::<Vec<_>>();
                    out["content"] = Value::Array(cleaned);
                }
                other => {
                    out["content"] = other.clone();
                }
            }
        }
        if let Some(tool_calls) = &self.tool_calls {
            out["tool_calls"] = Value::Array(tool_calls.clone());
        }
        if let Some(tool_call_id) = &self.tool_call_id {
            out["tool_call_id"] = json!(tool_call_id);
        }
        if let Some(name) = &self.name {
            out["name"] = json!(name);
        }
        if let Some(reasoning_content) = &self.reasoning_content {
            out["reasoning_content"] = json!(reasoning_content);
        }
        out
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub key: String,
    #[serde(default)]
    pub messages: Vec<ChatMessage>,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default)]
    pub metadata: BTreeMap<String, Value>,
    #[serde(default)]
    pub last_consolidated: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionSummary {
    pub key: String,
    pub updated_at: String,
    pub message_count: usize,
    pub last_consolidated: usize,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub context_tokens: Option<usize>,
}

impl Session {
    pub fn new(key: impl Into<String>) -> Self {
        let now = now_iso();
        Self {
            key: key.into(),
            messages: Vec::new(),
            created_at: now.clone(),
            updated_at: now,
            metadata: BTreeMap::new(),
            last_consolidated: 0,
        }
    }

    pub fn add_message(&mut self, role: &str, content: impl Into<String>) {
        self.messages.push(ChatMessage::text(role, content));
        self.updated_at = now_iso();
    }

    pub fn title(&self, max_len: usize) -> String {
        self.messages
            .iter()
            .find(|m| m.role == "user")
            .and_then(|m| match &m.content {
                Some(Value::String(s)) => Some(s.as_str()),
                Some(Value::Array(blocks)) => blocks
                    .iter()
                    .find_map(|b| b.get("text").and_then(Value::as_str)),
                _ => None,
            })
            .map(|text| {
                let first_line = text.lines().next().unwrap_or(text);
                if first_line.len() <= max_len {
                    first_line.to_string()
                } else {
                    format!("{}…", &first_line[..max_len.saturating_sub(1)])
                }
            })
            .unwrap_or_else(|| "(empty session)".to_string())
    }

    pub fn context_tokens(&self) -> Option<usize> {
        self.metadata
            .get(SESSION_CONTEXT_TOKENS_KEY)
            .and_then(Value::as_u64)
            .map(|value| value as usize)
            .filter(|value| *value > 0)
    }

    pub fn set_context_tokens(&mut self, tokens: usize) {
        if tokens > 0 {
            self.metadata.insert(
                SESSION_CONTEXT_TOKENS_KEY.to_string(),
                Value::from(tokens as u64),
            );
        }
    }

    pub fn clear(&mut self) {
        self.messages.clear();
        self.last_consolidated = 0;
        self.metadata.remove(SESSION_CONTEXT_TOKENS_KEY);
        self.updated_at = now_iso();
    }

    pub fn get_history(&self, max_messages: usize) -> Vec<ChatMessage> {
        let unconsolidated = &self.messages[self.last_consolidated.min(self.messages.len())..];
        let mut sliced = if max_messages == 0 || unconsolidated.len() <= max_messages {
            unconsolidated.to_vec()
        } else {
            unconsolidated[unconsolidated.len() - max_messages..].to_vec()
        };

        if let Some(idx) = sliced.iter().position(|message| message.role == "user") {
            sliced = sliced[idx..].to_vec();
        }

        let legal_start = find_legal_start(&sliced);
        if legal_start > 0 {
            sliced = sliced[legal_start..].to_vec();
        }

        sliced
            .into_iter()
            .map(|message| ChatMessage {
                role: message.role,
                content: message.content,
                tool_calls: message.tool_calls,
                tool_call_id: message.tool_call_id,
                name: message.name,
                timestamp: None,
                reasoning_content: message.reasoning_content,
                thinking_blocks: message.thinking_blocks,
                metadata: None,
            })
            .collect()
    }
}

fn find_legal_start(messages: &[ChatMessage]) -> usize {
    let mut declared: HashSet<String> = HashSet::new();
    let mut start = 0;
    for (idx, message) in messages.iter().enumerate() {
        match message.role.as_str() {
            "assistant" => {
                if let Some(tool_calls) = &message.tool_calls {
                    for tool_call in tool_calls {
                        if let Some(id) = tool_call.get("id").and_then(Value::as_str) {
                            declared.insert(id.to_string());
                        }
                    }
                }
            }
            "tool" => {
                if let Some(id) = &message.tool_call_id {
                    if !declared.contains(id) {
                        start = idx + 1;
                        declared.clear();
                        for previous in &messages[start..=idx] {
                            if previous.role == "assistant" {
                                if let Some(tool_calls) = &previous.tool_calls {
                                    for tool_call in tool_calls {
                                        if let Some(prev_id) =
                                            tool_call.get("id").and_then(Value::as_str)
                                        {
                                            declared.insert(prev_id.to_string());
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    start
}

#[cfg(test)]
mod tests {
    use super::ChatMessage;
    use serde_json::json;

    #[test]
    fn to_openai_payload_strips_meta() {
        let message = ChatMessage {
            role: "user".to_string(),
            content: Some(json!([
                {
                    "type": "image_url",
                    "image_url": {"url": "data:image/png;base64,encoded"},
                    "_meta": {"path": "test.png"}
                },
                {
                    "type": "text",
                    "text": "what is this?"
                }
            ])),
            tool_calls: None,
            tool_call_id: None,
            name: None,
            timestamp: None,
            reasoning_content: None,
            thinking_blocks: None,
            metadata: None,
        };

        let payload = message.to_openai_payload();
        let content = payload.get("content").unwrap().as_array().unwrap();

        assert_eq!(content.len(), 2);
        assert!(content[0].get("_meta").is_none());
        assert_eq!(
            content[0].get("image_url").unwrap().get("url").unwrap(),
            "data:image/png;base64,encoded"
        );
        assert_eq!(content[1].get("text").unwrap(), "what is this?");
    }

    #[test]
    fn to_openai_payload_preserves_role_and_text_content() {
        let message = ChatMessage::text("user", "hello");
        let payload = message.to_openai_payload();

        assert_eq!(payload.get("role").unwrap(), "user");
        assert_eq!(payload.get("content").unwrap(), "hello");
    }

    #[test]
    fn to_openai_payload_includes_reasoning_content() {
        let message = ChatMessage {
            role: "assistant".to_string(),
            content: Some(json!("The answer is 42")),
            tool_calls: None,
            tool_call_id: None,
            name: None,
            timestamp: None,
            reasoning_content: Some("Let me think step by step...".to_string()),
            thinking_blocks: None,
            metadata: None,
        };
        let payload = message.to_openai_payload();

        assert_eq!(payload.get("role").unwrap(), "assistant");
        assert_eq!(payload.get("content").unwrap(), "The answer is 42");
        assert_eq!(
            payload.get("reasoning_content").unwrap(),
            "Let me think step by step..."
        );
    }

    #[test]
    fn to_openai_payload_omits_reasoning_content_when_none() {
        let message = ChatMessage::text("assistant", "hello");
        let payload = message.to_openai_payload();

        assert!(payload.get("reasoning_content").is_none());
    }

    #[test]
    fn session_title_from_first_user_message() {
        use super::Session;
        let mut s = Session::new("test-key");
        assert_eq!(s.title(40), "(empty session)");

        s.add_message("user", "Find all bugs in the authentication module");
        s.add_message("assistant", "I'll look at that.");

        assert_eq!(s.title(40), "Find all bugs in the authentication mod…");
        assert_eq!(s.title(100), "Find all bugs in the authentication module");
    }

    #[test]
    fn session_title_from_multiline_uses_first_line() {
        use super::Session;
        let mut s = Session::new("test-key");
        s.add_message("user", "First line\nSecond line\nThird line");
        assert_eq!(s.title(100), "First line");
    }

    #[test]
    fn session_context_tokens_come_from_metadata() {
        use super::Session;
        let mut s = Session::new("test-key");
        assert_eq!(s.context_tokens(), None);

        s.add_message("user", "Hello world");
        assert_eq!(s.context_tokens(), None);

        s.set_context_tokens(1234);
        assert_eq!(s.context_tokens(), Some(1234));

        s.clear();
        assert_eq!(s.context_tokens(), None);
    }

    #[test]
    fn session_summary_includes_title_and_context_tokens() {
        use super::{Session, SessionManager};
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().to_path_buf();
        std::fs::create_dir_all(ws.join(".xbot").join("sessions")).unwrap();

        let mut manager = SessionManager::new(&ws).unwrap();
        let mut s1 = Session::new("cli:proj:aaa");
        s1.add_message("user", "Fix the login page");
        s1.add_message("assistant", "I'll fix it now.");
        s1.set_context_tokens(2048);
        manager.save(&s1).unwrap();

        let mut s2 = Session::new("cli:proj:bbb");
        s2.add_message("user", "Run tests for module X");
        manager.save(&s2).unwrap();

        let summaries = manager.list_session_summaries().unwrap();
        assert_eq!(summaries.len(), 2);

        for s in &summaries {
            assert!(!s.title.is_empty());
            assert_ne!(s.title, "(empty session)");
        }

        let fix_login = summaries.iter().find(|s| s.key == "cli:proj:aaa").unwrap();
        assert_eq!(fix_login.message_count, 2);
        assert_eq!(fix_login.title, "Fix the login page");
        assert_eq!(fix_login.context_tokens, Some(2048));

        let run_tests = summaries.iter().find(|s| s.key == "cli:proj:bbb").unwrap();
        assert_eq!(run_tests.context_tokens, None);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionMetadataLine {
    #[serde(rename = "_type")]
    kind: String,
    key: String,
    created_at: String,
    updated_at: String,
    #[serde(default)]
    metadata: BTreeMap<String, Value>,
    #[serde(default)]
    last_consolidated: usize,
}

pub struct SessionManager {
    sessions_dir: PathBuf,
    cache: BTreeMap<String, Session>,
}

impl SessionManager {
    pub fn new(workspace: &Path) -> Result<Self> {
        let sessions_dir = ensure_dir(workspace_state_dir(workspace).join("sessions"))?;
        Ok(Self {
            sessions_dir,
            cache: BTreeMap::new(),
        })
    }

    pub fn get_or_create(&mut self, key: &str) -> Result<Session> {
        if let Some(session) = self.cache.get(key) {
            return Ok(session.clone());
        }
        let session = self.load(key)?.unwrap_or_else(|| Session::new(key));
        self.cache.insert(key.to_string(), session.clone());
        Ok(session)
    }

    pub fn put(&mut self, session: Session) {
        self.cache.insert(session.key.clone(), session);
    }

    pub fn save(&mut self, session: &Session) -> Result<()> {
        let path = self.session_path(&session.key);
        let mut file = File::create(&path)
            .with_context(|| format!("failed to create session file {}", path.display()))?;
        let metadata = SessionMetadataLine {
            kind: "metadata".to_string(),
            key: session.key.clone(),
            created_at: session.created_at.clone(),
            updated_at: session.updated_at.clone(),
            metadata: session.metadata.clone(),
            last_consolidated: session.last_consolidated,
        };
        writeln!(file, "{}", serde_json::to_string(&metadata)?)?;
        for message in &session.messages {
            writeln!(file, "{}", serde_json::to_string(message)?)?;
        }
        self.cache.insert(session.key.clone(), session.clone());
        Ok(())
    }

    pub fn invalidate(&mut self, key: &str) {
        self.cache.remove(key);
    }

    pub fn list_sessions(&self) -> Result<Vec<String>> {
        let mut names = Vec::new();
        for entry in fs::read_dir(&self.sessions_dir)? {
            let entry = entry?;
            if entry.path().extension().and_then(|ext| ext.to_str()) == Some("jsonl") {
                names.push(entry.path().display().to_string());
            }
        }
        names.sort();
        Ok(names)
    }

    pub fn list_session_summaries(&self) -> Result<Vec<SessionSummary>> {
        let mut summaries = Vec::new();
        for entry in fs::read_dir(&self.sessions_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            if let Some(session) = self.load(stem)? {
                summaries.push(SessionSummary {
                    key: session.key.clone(),
                    updated_at: session.updated_at.clone(),
                    message_count: session.messages.len(),
                    last_consolidated: session.last_consolidated,
                    title: session.title(60),
                    context_tokens: session.context_tokens(),
                });
            }
        }
        summaries.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        Ok(summaries)
    }

    fn load(&self, key: &str) -> Result<Option<Session>> {
        let path = self.session_path(key);
        if !path.exists() {
            return Ok(None);
        }
        let file = File::open(&path)?;
        let reader = BufReader::new(file);
        let mut metadata: Option<SessionMetadataLine> = None;
        let mut messages = Vec::new();
        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let value: Value = serde_json::from_str(&line)?;
            if value.get("_type").and_then(Value::as_str) == Some("metadata") {
                metadata = Some(serde_json::from_value(value)?);
            } else {
                messages.push(serde_json::from_value(value)?);
            }
        }
        let meta = metadata.unwrap_or(SessionMetadataLine {
            kind: "metadata".to_string(),
            key: key.to_string(),
            created_at: now_iso(),
            updated_at: now_iso(),
            metadata: BTreeMap::new(),
            last_consolidated: 0,
        });
        Ok(Some(Session {
            key: meta.key,
            messages,
            created_at: meta.created_at,
            updated_at: meta.updated_at,
            metadata: meta.metadata,
            last_consolidated: meta.last_consolidated,
        }))
    }

    fn session_path(&self, key: &str) -> PathBuf {
        self.sessions_dir
            .join(format!("{}.jsonl", safe_filename(&key.replace(':', "_"))))
    }
}
