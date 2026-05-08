pub mod anthropic;
pub mod registry;
pub mod transcription;

pub use anthropic::{AnthropicProvider, DEFAULT_MODEL};

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::storage::ChatMessage;

fn is_transient_error_message(message: &str) -> bool {
    let lowered = message.to_ascii_lowercase();
    [
        "429",
        "500",
        "502",
        "503",
        "504",
        "rate limit",
        "overloaded",
        "timeout",
        "timed out",
        "temporarily unavailable",
        "connection reset",
        "connection refused",
        "broken pipe",
        "error decoding response body",
        "stream client disconnected",
        "server error",
    ]
    .iter()
    .any(|marker| lowered.contains(marker))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCallRequest {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

impl ToolCallRequest {
    pub fn to_openai_tool_call(&self) -> Value {
        json!({
            "id": self.id,
            "type": "function",
            "function": {
                "name": self.name,
                "arguments": self.arguments.to_string(),
            }
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LlmUsage {
    pub prompt_tokens: usize,
    pub completion_tokens: usize,
    #[serde(default)]
    pub cached_prompt_tokens: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmResponse {
    pub content: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<ToolCallRequest>,
    pub finish_reason: String,
    #[serde(default)]
    pub usage: LlmUsage,
    #[serde(default)]
    pub reasoning_content: Option<String>,
    #[serde(default)]
    pub thinking_blocks: Option<Vec<Value>>,
}

impl LlmResponse {
    pub fn has_tool_calls(&self) -> bool {
        !self.tool_calls.is_empty()
    }
}

pub type TextStreamCallback = Arc<Mutex<Box<dyn FnMut(String) + Send>>>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderModelInfo {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window_tokens: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct GenerationSettings {
    pub temperature: Option<f32>,
    pub max_tokens: usize,
}

impl Default for GenerationSettings {
    fn default() -> Self {
        Self {
            temperature: None,
            max_tokens: 16_384,
        }
    }
}

#[async_trait]
pub trait LlmProvider: Send + Sync {
    fn default_model(&self) -> &str;

    fn generation(&self) -> GenerationSettings {
        GenerationSettings::default()
    }

    async fn chat(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Value]>,
        model: Option<&str>,
        max_tokens: Option<usize>,
        temperature: Option<f32>,
    ) -> Result<LlmResponse>;

    async fn list_models(&self) -> Result<Vec<ProviderModelInfo>> {
        Err(anyhow!("listing models is not supported by this provider"))
    }

    async fn chat_stream(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Value]>,
        model: Option<&str>,
        max_tokens: Option<usize>,
        temperature: Option<f32>,
        text_stream: Option<TextStreamCallback>,
    ) -> Result<LlmResponse> {
        let response = self
            .chat(messages, tools, model, max_tokens, temperature)
            .await?;
        if let Some(content) = response.content.clone() {
            emit_text_delta(text_stream.as_ref(), &content);
        }
        Ok(response)
    }

    async fn chat_with_retry(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Value]>,
        model: Option<&str>,
        max_tokens: Option<usize>,
        temperature: Option<f32>,
    ) -> Result<LlmResponse> {
        let delays = [1_u64, 2, 4];
        let mut last_error: Option<anyhow::Error> = None;
        for (attempt, delay) in delays.into_iter().enumerate() {
            match self
                .chat(messages, tools, model, max_tokens, temperature)
                .await
            {
                Ok(response) => return Ok(response),
                Err(err) => {
                    let transient = is_transient_error_message(&err.to_string());
                    last_error = Some(err);
                    if !transient || attempt == 2 {
                        break;
                    }
                    tokio::time::sleep(Duration::from_secs(delay)).await;
                }
            }
        }
        Err(last_error.unwrap_or_else(|| anyhow!("provider request failed")))
    }

    async fn chat_with_retry_stream(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Value]>,
        model: Option<&str>,
        max_tokens: Option<usize>,
        temperature: Option<f32>,
        text_stream: Option<TextStreamCallback>,
    ) -> Result<LlmResponse> {
        let delays = [1_u64, 2, 4];
        let mut last_error: Option<anyhow::Error> = None;
        for (attempt, delay) in delays.into_iter().enumerate() {
            match self
                .chat_stream(
                    messages,
                    tools,
                    model,
                    max_tokens,
                    temperature,
                    text_stream.clone(),
                )
                .await
            {
                Ok(response) => return Ok(response),
                Err(err) => {
                    let transient = is_transient_error_message(&err.to_string());
                    last_error = Some(err);
                    if !transient || attempt == 2 {
                        break;
                    }
                    tokio::time::sleep(Duration::from_secs(delay)).await;
                }
            }
        }
        Err(last_error.unwrap_or_else(|| anyhow!("provider request failed")))
    }
}

pub struct OpenAiCompatibleProvider {
    client: Client,
    api_key: String,
    api_base: String,
    default_model: String,
    extra_headers: BTreeMap<String, String>,
    generation: GenerationSettings,
    reasoning_effort: Option<String>,
}

impl OpenAiCompatibleProvider {
    pub fn new(
        api_key: String,
        api_base: Option<String>,
        default_model: String,
        extra_headers: BTreeMap<String, String>,
        generation: GenerationSettings,
        proxy: Option<&str>,
    ) -> Result<Self> {
        Self::with_reasoning(
            api_key,
            api_base,
            default_model,
            extra_headers,
            generation,
            proxy,
            None,
        )
    }

    pub fn with_reasoning(
        api_key: String,
        api_base: Option<String>,
        default_model: String,
        extra_headers: BTreeMap<String, String>,
        generation: GenerationSettings,
        proxy: Option<&str>,
        reasoning_effort: Option<String>,
    ) -> Result<Self> {
        let mut builder = Client::builder().timeout(Duration::from_secs(600));
        if let Some(proxy) = proxy {
            builder = builder.proxy(reqwest::Proxy::all(proxy)?);
        }
        Ok(Self {
            client: builder.build()?,
            api_key,
            api_base: api_base.unwrap_or_else(|| "https://api.openai.com/v1".to_string()),
            default_model,
            extra_headers,
            generation,
            reasoning_effort,
        })
    }

    async fn list_models_inner(&self) -> Result<Vec<ProviderModelInfo>> {
        let endpoint = format!("{}/models", self.api_base.trim_end_matches('/'));
        let mut request = self.client.get(endpoint);
        if !self.api_key.trim().is_empty() {
            request = request.bearer_auth(&self.api_key);
        }
        for (key, value) in &self.extra_headers {
            request = request.header(key, value);
        }
        let response = request.send().await?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!("provider error {status}: {body}"));
        }
        let payload = response.json::<Value>().await?;
        let items = payload
            .get("data")
            .and_then(Value::as_array)
            .context("provider /models response missing data array")?;
        Ok(items
            .iter()
            .filter_map(|item| {
                let id = item.get("id").and_then(Value::as_str)?;
                Some(ProviderModelInfo {
                    id: id.to_string(),
                    context_window_tokens: extract_context_window_tokens(item),
                })
            })
            .collect())
    }
}

pub struct CustomProvider {
    inner: OpenAiCompatibleProvider,
}

impl CustomProvider {
    pub fn new(
        api_key: String,
        api_base: Option<String>,
        default_model: String,
        mut extra_headers: BTreeMap<String, String>,
        generation: GenerationSettings,
        proxy: Option<&str>,
    ) -> Result<Self> {
        extra_headers
            .entry("x-session-affinity".to_string())
            .or_insert_with(|| Uuid::new_v4().simple().to_string());
        Ok(Self {
            inner: OpenAiCompatibleProvider::new(
                api_key,
                api_base.or_else(|| Some("http://localhost:8000/v1".to_string())),
                default_model,
                extra_headers,
                generation,
                proxy,
            )?,
        })
    }
}

#[async_trait]
impl LlmProvider for CustomProvider {
    fn default_model(&self) -> &str {
        self.inner.default_model()
    }

    fn generation(&self) -> GenerationSettings {
        self.inner.generation()
    }

    async fn chat(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Value]>,
        model: Option<&str>,
        max_tokens: Option<usize>,
        temperature: Option<f32>,
    ) -> Result<LlmResponse> {
        self.inner
            .chat(messages, tools, model, max_tokens, temperature)
            .await
    }

    async fn chat_stream(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Value]>,
        model: Option<&str>,
        max_tokens: Option<usize>,
        temperature: Option<f32>,
        text_stream: Option<TextStreamCallback>,
    ) -> Result<LlmResponse> {
        self.inner
            .chat_stream(messages, tools, model, max_tokens, temperature, text_stream)
            .await
    }

    async fn list_models(&self) -> Result<Vec<ProviderModelInfo>> {
        self.inner.list_models().await
    }
}

pub struct AzureOpenAiProvider {
    client: Client,
    api_key: String,
    api_base: String,
    default_model: String,
    generation: GenerationSettings,
}

impl AzureOpenAiProvider {
    pub fn new(
        api_key: String,
        api_base: String,
        default_model: String,
        generation: GenerationSettings,
        proxy: Option<&str>,
    ) -> Result<Self> {
        if api_key.trim().is_empty() {
            return Err(anyhow!("Azure OpenAI api_key is required"));
        }
        if api_base.trim().is_empty() {
            return Err(anyhow!("Azure OpenAI api_base is required"));
        }
        let mut builder = Client::builder().timeout(Duration::from_secs(600));
        if let Some(proxy) = proxy {
            builder = builder.proxy(reqwest::Proxy::all(proxy)?);
        }
        Ok(Self {
            client: builder.build()?,
            api_key,
            api_base: api_base.trim_end_matches('/').to_string(),
            default_model,
            generation,
        })
    }

    fn build_chat_url(&self, deployment_name: &str) -> String {
        format!(
            "{}/openai/deployments/{}/chat/completions?api-version=2024-10-21",
            self.api_base, deployment_name
        )
    }
}

#[async_trait]
impl LlmProvider for OpenAiCompatibleProvider {
    fn default_model(&self) -> &str {
        &self.default_model
    }

    fn generation(&self) -> GenerationSettings {
        self.generation.clone()
    }

    async fn chat(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Value]>,
        model: Option<&str>,
        max_tokens: Option<usize>,
        temperature: Option<f32>,
    ) -> Result<LlmResponse> {
        self.chat_stream(messages, tools, model, max_tokens, temperature, None)
            .await
    }

    async fn chat_stream(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Value]>,
        model: Option<&str>,
        max_tokens: Option<usize>,
        temperature: Option<f32>,
        text_stream: Option<TextStreamCallback>,
    ) -> Result<LlmResponse> {
        let endpoint = format!("{}/chat/completions", self.api_base.trim_end_matches('/'));
        let openai_messages: Vec<Value> = messages.iter().map(|m| m.to_openai_payload()).collect();
        let effective_temp = temperature.or(self.generation.temperature);
        let mut payload = json!({
            "model": model.unwrap_or(self.default_model()),
            "messages": openai_messages,
            "tools": tools.unwrap_or(&[]),
            "max_tokens": max_tokens.unwrap_or(self.generation.max_tokens),
            "stream": true,
            "stream_options": {
                "include_usage": true
            }
        });
        if let Some(temp) = effective_temp {
            payload["temperature"] = json!(temp);
        }
        if let Some(ref effort) = self.reasoning_effort {
            let effort = effort.trim();
            if !effort.is_empty() {
                payload["reasoning_effort"] = json!(effort);
            }
        }
        let mut request = self.client.post(endpoint).json(&payload);
        if !self.api_key.trim().is_empty() {
            request = request.bearer_auth(&self.api_key);
        }
        for (key, value) in &self.extra_headers {
            request = request.header(key, value);
        }
        let response = request.send().await?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!("provider error {status}: {body}"));
        }

        parse_openai_like_response_stream_first(response, text_stream.as_ref()).await
    }

    async fn list_models(&self) -> Result<Vec<ProviderModelInfo>> {
        self.list_models_inner().await
    }
}

#[async_trait]
impl LlmProvider for AzureOpenAiProvider {
    fn default_model(&self) -> &str {
        &self.default_model
    }

    fn generation(&self) -> GenerationSettings {
        self.generation.clone()
    }

    async fn chat(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Value]>,
        model: Option<&str>,
        max_tokens: Option<usize>,
        temperature: Option<f32>,
    ) -> Result<LlmResponse> {
        self.chat_stream(messages, tools, model, max_tokens, temperature, None)
            .await
    }

    async fn chat_stream(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Value]>,
        model: Option<&str>,
        max_tokens: Option<usize>,
        temperature: Option<f32>,
        text_stream: Option<TextStreamCallback>,
    ) -> Result<LlmResponse> {
        let endpoint = self.build_chat_url(model.unwrap_or(&self.default_model));
        let openai_messages: Vec<Value> = messages.iter().map(|m| m.to_openai_payload()).collect();
        let mut payload = json!({
            "messages": openai_messages,
            "max_completion_tokens": max_tokens.unwrap_or(self.generation.max_tokens),
            "stream": true,
        });
        let effective_temp = temperature.or(self.generation.temperature);
        if let Some(temp) = effective_temp {
            payload["temperature"] = json!(temp);
        }
        if let Some(tools) = tools {
            payload["tools"] = Value::Array(tools.to_vec());
            payload["tool_choice"] = json!("auto");
        }
        let response = self
            .client
            .post(endpoint)
            .header("api-key", &self.api_key)
            .header("Content-Type", "application/json")
            .header("x-session-affinity", Uuid::new_v4().simple().to_string())
            .json(&payload)
            .send()
            .await?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!("provider error {status}: {body}"));
        }
        parse_openai_like_response_stream_first(response, text_stream.as_ref()).await
    }

    async fn list_models(&self) -> Result<Vec<ProviderModelInfo>> {
        Err(anyhow!(
            "listing models is not supported for Azure OpenAI providers"
        ))
    }
}

async fn parse_openai_like_response_stream_first(
    mut response: reqwest::Response,
    text_stream: Option<&TextStreamCallback>,
) -> Result<LlmResponse> {
    let is_stream = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.contains("text/event-stream"))
        .unwrap_or(false);
    if !is_stream {
        let parsed = parse_openai_like_response(response.json().await?)?;
        if let Some(content) = parsed.content.clone() {
            emit_text_delta(text_stream, &content);
        }
        return Ok(parsed);
    }

    let mut state = OpenAiLikeStreamState::default();
    let mut buffer = String::new();
    loop {
        let next_chunk = match response.chunk().await {
            Ok(chunk) => chunk,
            Err(err) => {
                if state.has_partial_response() {
                    return Ok(state.into_response());
                }
                return Err(err.into());
            }
        };
        let Some(chunk) = next_chunk else {
            break;
        };
        buffer.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(event) = extract_next_sse_event(&mut buffer) {
            if let Err(err) = apply_openai_like_sse_event(&mut state, &event, text_stream) {
                if state.has_partial_response() {
                    return Ok(state.into_response());
                }
                return Err(err);
            }
        }
    }
    if !buffer.trim().is_empty() {
        if let Err(err) = apply_openai_like_sse_event(&mut state, &buffer, text_stream) {
            if state.has_partial_response() {
                return Ok(state.into_response());
            }
            return Err(err);
        }
    }
    Ok(state.into_response())
}

fn parse_openai_like_response(payload: Value) -> Result<LlmResponse> {
    let choice = payload
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .cloned()
        .ok_or_else(|| anyhow!("missing choice in provider response"))?;
    let message = choice
        .get("message")
        .cloned()
        .ok_or_else(|| anyhow!("missing message in provider response"))?;

    let content = message.get("content").and_then(|content| match content {
        Value::String(text) => Some(text.clone()),
        Value::Array(parts) => {
            let text = parts
                .iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n");
            (!text.is_empty()).then_some(text)
        }
        _ => None,
    });
    let mut tool_calls = Vec::new();
    if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
        for tool_call in calls {
            let id = tool_call
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let function = tool_call
                .get("function")
                .cloned()
                .unwrap_or_else(|| json!({}));
            let name = function
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let arguments = function
                .get("arguments")
                .and_then(Value::as_str)
                .and_then(|raw| serde_json::from_str(raw).ok())
                .unwrap_or_else(|| {
                    function
                        .get("arguments")
                        .cloned()
                        .unwrap_or_else(|| json!({}))
                });
            tool_calls.push(ToolCallRequest {
                id,
                name,
                arguments,
            });
        }
    }

    let usage = payload.get("usage").cloned().unwrap_or_else(|| json!({}));
    Ok(LlmResponse {
        content,
        tool_calls,
        finish_reason: choice
            .get("finish_reason")
            .and_then(Value::as_str)
            .unwrap_or("stop")
            .to_string(),
        usage: LlmUsage {
            prompt_tokens: usage
                .get("prompt_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0) as usize,
            completion_tokens: usage
                .get("completion_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0) as usize,
            cached_prompt_tokens: usage
                .get("prompt_tokens_details")
                .and_then(|d| d.get("cached_tokens"))
                .and_then(Value::as_u64)
                .unwrap_or(0) as usize,
        },
        reasoning_content: message
            .get("reasoning_content")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        thinking_blocks: message
            .get("thinking_blocks")
            .and_then(Value::as_array)
            .cloned(),
    })
}

#[cfg(test)]
fn parse_openai_like_sse_text(raw: &str) -> Result<LlmResponse> {
    let mut state = OpenAiLikeStreamState::default();
    let normalized = raw.replace("\r\n", "\n");
    for event in normalized.split("\n\n") {
        if let Err(err) = apply_openai_like_sse_event(&mut state, event, None) {
            if state.has_partial_response() {
                return Ok(state.into_response());
            }
            return Err(err);
        }
    }
    Ok(state.into_response())
}

fn extract_delta_text(content: Option<&Value>) -> Option<String> {
    match content {
        Some(Value::String(text)) => Some(text.clone()),
        Some(Value::Array(parts)) => {
            let text = parts
                .iter()
                .filter_map(|part| {
                    part.get("text")
                        .and_then(Value::as_str)
                        .or_else(|| part.get("content").and_then(Value::as_str))
                })
                .collect::<Vec<_>>()
                .join("\n");
            (!text.is_empty()).then_some(text)
        }
        _ => None,
    }
}

#[derive(Default)]
struct PartialToolCall {
    id: String,
    name: String,
    arguments: String,
}

#[derive(Default)]
struct OpenAiLikeStreamState {
    content: String,
    finish_reason: String,
    usage: LlmUsage,
    reasoning_content: String,
    thinking_blocks: Vec<Value>,
    tool_calls: BTreeMap<usize, PartialToolCall>,
}

impl OpenAiLikeStreamState {
    fn has_partial_response(&self) -> bool {
        !self.content.trim().is_empty()
            || !self.reasoning_content.trim().is_empty()
            || !self.thinking_blocks.is_empty()
            || !self.tool_calls.is_empty()
            || self.usage.prompt_tokens > 0
            || self.usage.completion_tokens > 0
            || !self.finish_reason.is_empty()
    }

    fn into_response(self) -> LlmResponse {
        let parsed_tool_calls = self
            .tool_calls
            .into_values()
            .map(|call| ToolCallRequest {
                id: call.id,
                name: call.name,
                arguments: serde_json::from_str(&call.arguments)
                    .unwrap_or_else(|_| Value::String(call.arguments)),
            })
            .collect::<Vec<_>>();

        LlmResponse {
            content: (!self.content.trim().is_empty()).then_some(self.content.trim().to_string()),
            tool_calls: parsed_tool_calls,
            finish_reason: if self.finish_reason.is_empty() {
                "stop".to_string()
            } else {
                self.finish_reason
            },
            usage: self.usage,
            reasoning_content: (!self.reasoning_content.is_empty())
                .then_some(self.reasoning_content),
            thinking_blocks: (!self.thinking_blocks.is_empty()).then_some(self.thinking_blocks),
        }
    }

    fn push_content_update(&mut self, text: &str) -> String {
        let delta = normalize_stream_text_update(&self.content, text);
        if delta.len() == text.len() {
            self.content.push_str(text);
        } else if text.len() > self.content.len() && text.starts_with(&self.content) {
            self.content = text.to_string();
        }
        delta
    }
}

fn normalize_stream_text_update(current: &str, update: &str) -> String {
    if update.is_empty() {
        return String::new();
    }
    if current.is_empty() {
        return update.to_string();
    }
    if update.starts_with(current) {
        return update[current.len()..].to_string();
    }
    if current.starts_with(update) {
        return String::new();
    }
    update.to_string()
}

fn emit_text_delta(text_stream: Option<&TextStreamCallback>, delta: &str) {
    if delta.is_empty() {
        return;
    }
    if let Some(text_stream) = text_stream {
        let mut callback = text_stream.lock().expect("text stream lock poisoned");
        (callback)(delta.to_string());
    }
}

fn extract_next_sse_event(buffer: &mut String) -> Option<String> {
    let unix = buffer.find("\n\n");
    let windows = buffer.find("\r\n\r\n");
    let (index, separator_len) = match (unix, windows) {
        (Some(unix), Some(windows)) if unix <= windows => (unix, 2),
        (Some(_unix), Some(windows)) => (windows, 4),
        (Some(unix), None) => (unix, 2),
        (None, Some(windows)) => (windows, 4),
        (None, None) => return None,
    };
    let event = buffer[..index].to_string();
    buffer.drain(..index + separator_len);
    Some(event)
}

fn apply_openai_like_sse_event(
    state: &mut OpenAiLikeStreamState,
    event: &str,
    text_stream: Option<&TextStreamCallback>,
) -> Result<()> {
    let data_lines = event
        .lines()
        .filter_map(|line| line.strip_prefix("data:").map(str::trim))
        .collect::<Vec<_>>();
    if data_lines.is_empty() {
        return Ok(());
    }
    let data = data_lines.join("\n");
    if data == "[DONE]" {
        return Ok(());
    }
    let payload: Value = serde_json::from_str(&data)?;
    if let Some(usage_payload) = payload.get("usage") {
        state.usage.prompt_tokens = usage_payload
            .get("prompt_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(state.usage.prompt_tokens as u64)
            as usize;
        state.usage.completion_tokens = usage_payload
            .get("completion_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(state.usage.completion_tokens as u64)
            as usize;
        if let Some(details) = usage_payload.get("prompt_tokens_details") {
            state.usage.cached_prompt_tokens = details
                .get("cached_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0) as usize;
        }
    }
    let Some(choice) = payload
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
    else {
        return Ok(());
    };
    if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
        state.finish_reason = reason.to_string();
    }
    let Some(delta) = choice.get("delta") else {
        return Ok(());
    };
    if let Some(text) = extract_delta_text(delta.get("content")) {
        let text_delta = state.push_content_update(&text);
        emit_text_delta(text_stream, &text_delta);
    }
    if let Some(reasoning) = delta.get("reasoning_content").and_then(Value::as_str) {
        state.reasoning_content.push_str(reasoning);
        emit_text_delta(text_stream, &format!("\x1b[2;3m{reasoning}\x1b[0m"));
    }
    if let Some(blocks) = delta.get("thinking_blocks").and_then(Value::as_array) {
        state.thinking_blocks.extend(blocks.iter().cloned());
    }
    if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
        for call in calls {
            let index = call
                .get("index")
                .and_then(Value::as_u64)
                .unwrap_or(state.tool_calls.len() as u64) as usize;
            let entry = state.tool_calls.entry(index).or_default();
            if let Some(id) = call.get("id").and_then(Value::as_str) {
                entry.id = id.to_string();
            }
            if let Some(name) = call
                .get("function")
                .and_then(|function| function.get("name"))
                .and_then(Value::as_str)
            {
                entry.name.push_str(name);
            }
            if let Some(arguments) = call
                .get("function")
                .and_then(|function| function.get("arguments"))
                .and_then(Value::as_str)
            {
                entry.arguments.push_str(arguments);
            }
        }
    }
    Ok(())
}

fn extract_context_window_tokens(value: &Value) -> Option<usize> {
    const KEYS: &[&str] = &[
        "context_length",
        "max_context_length",
        "max_model_len",
        "max_sequence_length",
        "max_seq_len",
        "context_window",
        "num_ctx",
        "n_ctx",
    ];
    match value {
        Value::Object(map) => {
            for key in KEYS {
                if let Some(parsed) = map.get(*key).and_then(parse_context_value) {
                    return Some(parsed);
                }
            }
            for child in map.values() {
                if let Some(parsed) = extract_context_window_tokens(child) {
                    return Some(parsed);
                }
            }
            None
        }
        Value::Array(items) => items.iter().find_map(extract_context_window_tokens),
        _ => None,
    }
}

fn parse_context_value(value: &Value) -> Option<usize> {
    match value {
        Value::Number(number) => number.as_u64().map(|value| value as usize),
        Value::String(text) => {
            let digits = text
                .chars()
                .filter(|ch| ch.is_ascii_digit())
                .collect::<String>();
            digits.parse::<usize>().ok().filter(|value| *value > 0)
        }
        _ => None,
    }
}

#[derive(Default)]
pub struct QueuedProvider {
    queue: Mutex<VecDeque<LlmResponse>>,
    model: String,
}

impl QueuedProvider {
    pub fn new(model: impl Into<String>, responses: Vec<LlmResponse>) -> Self {
        Self {
            queue: Mutex::new(VecDeque::from(responses)),
            model: model.into(),
        }
    }

    pub fn push(&self, response: LlmResponse) {
        self.queue
            .lock()
            .expect("queue lock poisoned")
            .push_back(response);
    }
}

#[async_trait]
impl LlmProvider for QueuedProvider {
    fn default_model(&self) -> &str {
        &self.model
    }

    async fn chat(
        &self,
        _messages: &[ChatMessage],
        _tools: Option<&[Value]>,
        _model: Option<&str>,
        _max_tokens: Option<usize>,
        _temperature: Option<f32>,
    ) -> Result<LlmResponse> {
        self.queue
            .lock()
            .expect("queue lock poisoned")
            .pop_front()
            .context("queued provider exhausted")
    }

    async fn list_models(&self) -> Result<Vec<ProviderModelInfo>> {
        Ok(vec![ProviderModelInfo {
            id: self.model.clone(),
            context_window_tokens: None,
        }])
    }
}

pub type SharedProvider = Arc<dyn LlmProvider>;

#[cfg(test)]
mod tests {
    use super::{
        LlmProvider, LlmResponse, LlmUsage, ToolCallRequest, is_transient_error_message,
        normalize_stream_text_update, parse_openai_like_sse_text,
    };
    use crate::storage::ChatMessage;
    use anyhow::{Result, anyhow};
    use async_trait::async_trait;
    use serde_json::Value;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct FlakyProvider {
        attempts: AtomicUsize,
        transient_failures: usize,
    }

    #[async_trait]
    impl LlmProvider for FlakyProvider {
        fn default_model(&self) -> &str {
            "test-model"
        }

        async fn chat(
            &self,
            _messages: &[ChatMessage],
            _tools: Option<&[Value]>,
            _model: Option<&str>,
            _max_tokens: Option<usize>,
            _temperature: Option<f32>,
        ) -> Result<LlmResponse> {
            let attempt = self.attempts.fetch_add(1, Ordering::SeqCst);
            if attempt < self.transient_failures {
                return Err(anyhow!("provider error 503: upstream overloaded"));
            }
            Ok(LlmResponse {
                content: Some("ok".to_string()),
                tool_calls: Vec::<ToolCallRequest>::new(),
                finish_reason: "stop".to_string(),
                usage: LlmUsage {
                    prompt_tokens: 10,
                    completion_tokens: 5,
                    ..Default::default()
                },
                reasoning_content: None,
                thinking_blocks: None,
            })
        }
    }

    #[tokio::test]
    async fn retries_only_transient_errors() {
        let provider = FlakyProvider {
            attempts: AtomicUsize::new(0),
            transient_failures: 2,
        };
        let result = provider
            .chat_with_retry(
                &[ChatMessage::text("user", "hello")],
                None,
                None,
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(result.content.as_deref(), Some("ok"));
        assert_eq!(provider.attempts.load(Ordering::SeqCst), 3);
        assert!(is_transient_error_message("provider error 503"));
        assert!(!is_transient_error_message("schema mismatch"));
    }

    #[test]
    fn parses_streaming_openai_chunks() {
        let response = parse_openai_like_sse_text(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hello \"},\"finish_reason\":null}]}\n\n\
             data: {\"choices\":[{\"delta\":{\"content\":\"world\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":4}}\n\n\
             data: [DONE]\n\n",
        )
        .unwrap();
        assert_eq!(response.content.as_deref(), Some("Hello world"));
        assert_eq!(response.usage.prompt_tokens, 12);
        assert_eq!(response.usage.completion_tokens, 4);
    }

    #[test]
    fn parses_cumulative_openai_compatible_stream_chunks() {
        let response = parse_openai_like_sse_text(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n\
             data: {\"choices\":[{\"delta\":{\"content\":\"Hello world\"},\"finish_reason\":null}]}\n\n\
             data: {\"choices\":[{\"delta\":{\"content\":\"Hello world\"},\"finish_reason\":null}]}\n\n\
             data: {\"choices\":[{\"delta\":{\"content\":\"Hello world!\"},\"finish_reason\":\"stop\"}]}\n\n\
             data: [DONE]\n\n",
        )
        .unwrap();
        assert_eq!(response.content.as_deref(), Some("Hello world!"));
    }

    #[test]
    fn normalizes_cumulative_stream_updates_to_suffixes() {
        assert_eq!(normalize_stream_text_update("", "Hello"), "Hello");
        assert_eq!(
            normalize_stream_text_update("Hello", "Hello world"),
            " world"
        );
        assert_eq!(
            normalize_stream_text_update("Hello world", "Hello world"),
            ""
        );
        assert_eq!(normalize_stream_text_update("Hello", " there"), " there");
    }

    #[test]
    fn salvages_partial_stream_when_tail_is_malformed() {
        let response = parse_openai_like_sse_text(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Partial answer\"},\"finish_reason\":null}]}\n\n\
             data: {\"choices\":[{\"delta\":",
        )
        .unwrap();
        assert_eq!(response.content.as_deref(), Some("Partial answer"));
    }

    #[test]
    fn treats_body_decode_errors_as_transient() {
        assert!(is_transient_error_message("error decoding response body"));
    }
}
