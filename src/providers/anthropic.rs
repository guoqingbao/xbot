//! Native Anthropic Messages API (`/v1/messages`) provider.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use reqwest::Client;
use serde_json::{Value, json};

use crate::providers::{
    GenerationSettings, LlmProvider, LlmResponse, LlmUsage, ProviderModelInfo, TextStreamCallback,
    ToolCallRequest,
};
use crate::storage::ChatMessage;

const ANTHROPIC_VERSION: &str = "2023-06-01";
const DEFAULT_API_BASE: &str = "https://api.anthropic.com";
pub const DEFAULT_MODEL: &str = "claude-sonnet-4-20250514";

/// Default `anthropic-beta` value when `reasoning_effort` is set to a non-empty placeholder
/// (e.g. `"high"`) and no explicit beta string is provided in config.
const DEFAULT_THINKING_BETA: &str = "interleaved-thinking-2025-05-14";

pub struct AnthropicProvider {
    client: Client,
    api_key: String,
    api_base: String,
    default_model: String,
    extra_headers: BTreeMap<String, String>,
    generation: GenerationSettings,
    /// When set, sends `anthropic-beta` for extended thinking (unless already in `extra_headers`).
    reasoning_effort: Option<String>,
}

impl AnthropicProvider {
    pub fn new(
        api_key: String,
        api_base: Option<String>,
        default_model: String,
        extra_headers: BTreeMap<String, String>,
        generation: GenerationSettings,
        proxy: Option<&str>,
        reasoning_effort: Option<String>,
    ) -> Result<Self> {
        if api_key.trim().is_empty() {
            return Err(anyhow!("Anthropic api_key is required"));
        }
        let mut builder = Client::builder().timeout(Duration::from_secs(600));
        if let Some(proxy) = proxy {
            builder = builder.proxy(reqwest::Proxy::all(proxy)?);
        }
        let base = api_base
            .unwrap_or_else(|| DEFAULT_API_BASE.to_string())
            .trim_end_matches('/')
            .to_string();
        Ok(Self {
            client: builder.build()?,
            api_key,
            api_base: base,
            default_model,
            extra_headers,
            generation,
            reasoning_effort,
        })
    }

    fn messages_url(&self) -> String {
        if self.api_base.ends_with("/v1") {
            format!("{}/messages", self.api_base)
        } else {
            format!("{}/v1/messages", self.api_base)
        }
    }

    fn models_url(&self) -> String {
        if self.api_base.ends_with("/v1") {
            format!("{}/models", self.api_base)
        } else {
            format!("{}/v1/models", self.api_base)
        }
    }

    fn apply_standard_headers(
        &self,
        mut request: reqwest::RequestBuilder,
    ) -> reqwest::RequestBuilder {
        request = request
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json");

        let has_beta = self
            .extra_headers
            .keys()
            .any(|k| k.eq_ignore_ascii_case("anthropic-beta"));

        for (key, value) in &self.extra_headers {
            request = request.header(key, value);
        }

        if !has_beta {
            if let Some(ref effort) = self.reasoning_effort {
                let t = effort.trim();
                if !t.is_empty() {
                    let beta = if t.contains('-') && t.chars().any(|c| c.is_ascii_digit()) {
                        t.to_string()
                    } else {
                        DEFAULT_THINKING_BETA.to_string()
                    };
                    request = request.header("anthropic-beta", beta);
                }
            }
        }

        request
    }

    async fn send(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Value]>,
        model: Option<&str>,
        max_tokens: Option<usize>,
        temperature: Option<f32>,
        stream: bool,
        text_stream: Option<TextStreamCallback>,
    ) -> Result<LlmResponse> {
        let (system, anthropic_messages) = convert_messages(messages)?;
        let model_id =
            normalize_model_id(model.unwrap_or(&self.default_model), &self.default_model);
        let max_t = max_tokens.unwrap_or(self.generation.max_tokens);
        let effective_temp = temperature.or(self.generation.temperature);

        let mut body = json!({
            "model": model_id,
            "max_tokens": max_t,
            "messages": anthropic_messages,
            "stream": stream,
        });
        if let Some(temp) = effective_temp {
            body["temperature"] = json!(temp);
        }
        if let Some(sys) = system {
            body["system"] = json!(sys);
        }
        if let Some(tools) = tools {
            let converted = convert_openai_tools_to_anthropic(tools);
            if !converted.is_empty() {
                body["tools"] = Value::Array(converted);
            }
        }

        let request = self
            .apply_standard_headers(self.client.post(self.messages_url()).json(&body))
            .build()
            .context("anthropic request build failed")?;

        let response = self.client.execute(request).await?;
        if !response.status().is_success() {
            let status = response.status();
            let err_body = response.text().await.unwrap_or_default();
            return Err(anyhow!("Anthropic API error {status}: {err_body}"));
        }

        if stream {
            parse_streaming_response(response, text_stream.as_ref()).await
        } else {
            let value: Value = response.json().await?;
            parse_non_streaming_message(&value)
        }
    }
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
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
        self.send(messages, tools, model, max_tokens, temperature, false, None)
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
        self.send(
            messages,
            tools,
            model,
            max_tokens,
            temperature,
            true,
            text_stream,
        )
        .await
    }

    async fn list_models(&self) -> Result<Vec<ProviderModelInfo>> {
        let request = self
            .apply_standard_headers(self.client.get(self.models_url()))
            .build()
            .context("anthropic list models request build failed")?;
        let response = self.client.execute(request).await?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!("Anthropic /models error {status}: {body}"));
        }
        let payload: Value = response.json().await?;
        let items = payload
            .get("data")
            .and_then(Value::as_array)
            .context("anthropic /models response missing data array")?;
        Ok(items
            .iter()
            .filter_map(|item| {
                let id = item.get("id").and_then(Value::as_str)?;
                Some(ProviderModelInfo {
                    id: id.to_string(),
                    context_window_tokens: None,
                })
            })
            .collect())
    }
}

fn normalize_model_id(model: &str, default: &str) -> String {
    let m = model.trim();
    if m.is_empty() {
        return default.to_string();
    }
    if let Some(rest) = m.strip_prefix("anthropic/") {
        return rest.to_string();
    }
    m.to_string()
}

fn convert_openai_tools_to_anthropic(tools: &[Value]) -> Vec<Value> {
    tools
        .iter()
        .filter_map(|t| {
            let func = t.get("function")?;
            let name = func.get("name")?.as_str()?;
            let description = func
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("");
            let input_schema = func
                .get("parameters")
                .cloned()
                .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
            Some(json!({
                "name": name,
                "description": description,
                "input_schema": input_schema,
            }))
        })
        .collect()
}

fn convert_messages(messages: &[ChatMessage]) -> Result<(Option<String>, Vec<Value>)> {
    let mut system_parts: Vec<String> = Vec::new();
    let mut out: Vec<Value> = Vec::new();
    let mut pending_tool_results: Vec<Value> = Vec::new();

    for msg in messages {
        if msg.role == "tool" {
            pending_tool_results.push(tool_message_to_block(msg)?);
            continue;
        }

        if !pending_tool_results.is_empty() {
            out.push(json!({
                "role": "user",
                "content": Value::Array(std::mem::take(&mut pending_tool_results)),
            }));
        }

        match msg.role.as_str() {
            "system" => {
                if let Some(s) = msg.content_as_text() {
                    if !s.trim().is_empty() {
                        system_parts.push(s);
                    }
                }
            }
            "user" => {
                let content = msg
                    .content
                    .as_ref()
                    .map(convert_user_content_openai_to_anthropic)
                    .unwrap_or_else(|| Value::String(String::new()));
                out.push(json!({"role": "user", "content": content}));
            }
            "assistant" => {
                out.push(assistant_message_to_anthropic(msg)?);
            }
            other => {
                // Map unknown roles (e.g. "developer") as user text for compatibility.
                let text = msg.content_as_text().unwrap_or_default();
                if !text.trim().is_empty() {
                    out.push(json!({
                        "role": "user",
                        "content": format!("[{other}]\n{text}"),
                    }));
                }
            }
        }
    }

    if !pending_tool_results.is_empty() {
        out.push(json!({
            "role": "user",
            "content": Value::Array(pending_tool_results),
        }));
    }

    let system = if system_parts.is_empty() {
        None
    } else {
        Some(system_parts.join("\n\n"))
    };

    Ok((system, out))
}

fn tool_message_to_block(msg: &ChatMessage) -> Result<Value> {
    let tool_use_id = msg
        .tool_call_id
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("tool message missing tool_call_id"))?;
    let content = tool_result_content_to_anthropic(&msg.content);
    Ok(json!({
        "type": "tool_result",
        "tool_use_id": tool_use_id,
        "content": content,
    }))
}

fn tool_result_content_to_anthropic(content: &Option<Value>) -> Value {
    match content {
        None => Value::String(String::new()),
        Some(Value::String(s)) => Value::String(s.clone()),
        Some(v) => Value::String(v.to_string()),
    }
}

fn assistant_message_to_anthropic(msg: &ChatMessage) -> Result<Value> {
    let mut blocks: Vec<Value> = Vec::new();

    if let Some(text) = assistant_text_from_content(msg) {
        if !text.is_empty() {
            blocks.push(json!({"type": "text", "text": text}));
        }
    }

    if let Some(calls) = &msg.tool_calls {
        for call in calls {
            if let Some(block) = openai_tool_call_to_anthropic_tool_use(call)? {
                blocks.push(block);
            }
        }
    }

    if blocks.is_empty() {
        blocks.push(json!({"type": "text", "text": ""}));
    }

    Ok(json!({
        "role": "assistant",
        "content": Value::Array(blocks),
    }))
}

fn assistant_text_from_content(msg: &ChatMessage) -> Option<String> {
    msg.content_as_text()
}

fn openai_tool_call_to_anthropic_tool_use(call: &Value) -> Result<Option<Value>> {
    let id = call
        .get("id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("tool call missing id"))?;
    let function = call.get("function").cloned().unwrap_or_else(|| json!({}));
    let name = function.get("name").and_then(Value::as_str).unwrap_or("");
    let arguments = function
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let input: Value = match arguments {
        Value::String(raw) => serde_json::from_str(&raw).unwrap_or_else(|_| json!({})),
        other => other,
    };
    Ok(Some(json!({
        "type": "tool_use",
        "id": id,
        "name": name,
        "input": input,
    })))
}

fn convert_user_content_openai_to_anthropic(content: &Value) -> Value {
    match content {
        Value::String(s) => Value::String(s.clone()),
        Value::Array(parts) => {
            let mut blocks = Vec::new();
            for part in parts {
                let part = match part.as_object() {
                    Some(o) if o.contains_key("_meta") => {
                        let mut p = part.clone();
                        if let Some(m) = p.as_object_mut() {
                            m.remove("_meta");
                        }
                        p
                    }
                    _ => part.clone(),
                };
                match part.get("type").and_then(Value::as_str) {
                    Some("text") => blocks.push(part.clone()),
                    Some("image_url") => {
                        if let Some(url) = part
                            .get("image_url")
                            .and_then(|i| i.get("url"))
                            .and_then(Value::as_str)
                        {
                            if let Some((mime, data)) = parse_data_url_base64(url) {
                                blocks.push(json!({
                                    "type": "image",
                                    "source": {
                                        "type": "base64",
                                        "media_type": mime,
                                        "data": data,
                                    }
                                }));
                            }
                        }
                    }
                    _ => {}
                }
            }
            if blocks.is_empty() {
                Value::String(String::new())
            } else if blocks.len() == 1 {
                if let Some(t) = blocks[0].get("type").and_then(Value::as_str) {
                    if t == "text" {
                        return blocks[0]
                            .get("text")
                            .cloned()
                            .unwrap_or_else(|| Value::String(String::new()));
                    }
                }
                Value::Array(blocks)
            } else {
                Value::Array(blocks)
            }
        }
        other => other.clone(),
    }
}

fn parse_data_url_base64(url: &str) -> Option<(String, String)> {
    let prefix = "data:";
    let rest = url.strip_prefix(prefix)?;
    let (meta, b64) = rest.split_once(',')?;
    let mime = meta.split(';').next()?.to_string();
    if meta.contains("base64") {
        Some((mime, b64.to_string()))
    } else {
        None
    }
}

fn parse_non_streaming_message(payload: &Value) -> Result<LlmResponse> {
    let content_arr = payload
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("anthropic response missing content array"))?;

    let mut text = String::new();
    let mut tool_calls = Vec::new();
    let mut reasoning = String::new();
    let mut thinking_blocks: Vec<Value> = Vec::new();

    for block in content_arr {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(t) = block.get("text").and_then(Value::as_str) {
                    text.push_str(t);
                }
            }
            Some("tool_use") | Some("server_tool_use") => {
                let id = block
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let name = block
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let arguments = block.get("input").cloned().unwrap_or_else(|| json!({}));
                tool_calls.push(ToolCallRequest {
                    id,
                    name,
                    arguments,
                });
            }
            Some("thinking") => {
                let think_text = block
                    .get("thinking")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                reasoning.push_str(think_text);
                thinking_blocks.push(block.clone());
            }
            _ => {}
        }
    }

    let usage = payload.get("usage").cloned().unwrap_or_else(|| json!({}));
    let prompt_tokens = usage
        .get("input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;
    let completion_tokens = usage
        .get("output_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;

    let stop = payload
        .get("stop_reason")
        .and_then(Value::as_str)
        .unwrap_or("end_turn");
    let finish_reason = map_stop_reason(stop);

    Ok(LlmResponse {
        content: (!text.trim().is_empty()).then_some(text),
        tool_calls,
        finish_reason,
        usage: LlmUsage {
            prompt_tokens,
            completion_tokens,
            cached_prompt_tokens: usage
                .get("cache_read_input_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0) as usize,
        },
        reasoning_content: (!reasoning.trim().is_empty()).then_some(reasoning),
        thinking_blocks: (!thinking_blocks.is_empty()).then_some(thinking_blocks),
    })
}

fn map_stop_reason(stop: &str) -> String {
    match stop {
        "tool_use" => "tool_calls".to_string(),
        "end_turn" | "stop_sequence" => "stop".to_string(),
        "max_tokens" => "length".to_string(),
        other => other.to_string(),
    }
}

fn emit_text_delta(text_stream: Option<&TextStreamCallback>, delta: &str) {
    if delta.is_empty() {
        return;
    }
    if let Some(cb) = text_stream {
        let mut guard = cb.lock().expect("text stream lock poisoned");
        (guard)(delta.to_string());
    }
}

async fn parse_streaming_response(
    mut response: reqwest::Response,
    text_stream: Option<&TextStreamCallback>,
) -> Result<LlmResponse> {
    let mut state = AnthropicStreamState::default();
    let mut buffer = String::new();

    loop {
        let chunk = match response.chunk().await {
            Ok(c) => c,
            Err(err) => {
                if state.has_partial() {
                    return Ok(state.into_response());
                }
                return Err(err.into());
            }
        };
        let Some(chunk) = chunk else { break };
        buffer.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(event) = extract_next_sse_event(&mut buffer) {
            if let Err(err) = apply_anthropic_sse_event(&mut state, &event, text_stream) {
                if state.has_partial() {
                    return Ok(state.into_response());
                }
                return Err(err);
            }
        }
    }

    if !buffer.trim().is_empty() {
        if let Err(err) = apply_anthropic_sse_event(&mut state, &buffer, text_stream) {
            if state.has_partial() {
                return Ok(state.into_response());
            }
            return Err(err);
        }
    }

    Ok(state.into_response())
}

#[derive(Default)]
struct AnthropicStreamState {
    text: String,
    reasoning: String,
    thinking_blocks: Vec<Value>,
    tool_calls: Vec<ToolCallRequest>,
    finish_reason: String,
    usage: LlmUsage,
    blocks: BTreeMap<usize, PartialBlock>,
}

enum PartialBlock {
    Text,
    ToolUse {
        id: String,
        name: String,
        input_json: String,
    },
    Thinking {
        text: String,
    },
}

impl AnthropicStreamState {
    fn has_partial(&self) -> bool {
        !self.text.trim().is_empty()
            || !self.reasoning.trim().is_empty()
            || !self.thinking_blocks.is_empty()
            || !self.tool_calls.is_empty()
            || self.usage.prompt_tokens > 0
            || self.usage.completion_tokens > 0
            || !self.finish_reason.is_empty()
    }

    fn into_response(self) -> LlmResponse {
        let finish = if self.finish_reason.is_empty() {
            if self.tool_calls.is_empty() {
                "stop".to_string()
            } else {
                "tool_calls".to_string()
            }
        } else {
            self.finish_reason.clone()
        };

        LlmResponse {
            content: (!self.text.trim().is_empty()).then_some(self.text),
            tool_calls: self.tool_calls,
            finish_reason: finish,
            usage: self.usage,
            reasoning_content: (!self.reasoning.trim().is_empty()).then_some(self.reasoning),
            thinking_blocks: (!self.thinking_blocks.is_empty()).then_some(self.thinking_blocks),
        }
    }
}

fn extract_next_sse_event(buffer: &mut String) -> Option<String> {
    let unix = buffer.find("\n\n");
    let windows = buffer.find("\r\n\r\n");
    let (index, separator_len) = match (unix, windows) {
        (Some(u), Some(w)) if u <= w => (u, 2),
        (Some(_), Some(w)) => (w, 4),
        (Some(u), None) => (u, 2),
        (None, Some(w)) => (w, 4),
        (None, None) => return None,
    };
    let event = buffer[..index].to_string();
    buffer.drain(..index + separator_len);
    Some(event)
}

fn apply_anthropic_sse_event(
    state: &mut AnthropicStreamState,
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
    let payload: Value = serde_json::from_str(&data)?;

    match payload.get("type").and_then(Value::as_str) {
        Some("error") => {
            let msg = payload
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("unknown Anthropic stream error");
            return Err(anyhow!("Anthropic stream error: {msg}"));
        }
        Some("message_start") => {
            if let Some(usage) = payload.get("message").and_then(|m| m.get("usage")) {
                merge_usage(state, usage);
            }
        }
        Some("content_block_start") => {
            let index = payload.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
            if let Some(block) = payload.get("content_block") {
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        state.blocks.insert(index, PartialBlock::Text);
                    }
                    Some("tool_use") | Some("server_tool_use") => {
                        let id = block
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        let name = block
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        state.blocks.insert(
                            index,
                            PartialBlock::ToolUse {
                                id,
                                name,
                                input_json: String::new(),
                            },
                        );
                    }
                    Some("thinking") => {
                        state.blocks.insert(
                            index,
                            PartialBlock::Thinking {
                                text: String::new(),
                            },
                        );
                    }
                    _ => {}
                }
            }
        }
        Some("content_block_delta") => {
            let index = payload.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
            let delta = payload.get("delta");
            let delta_type = delta.and_then(|d| d.get("type")).and_then(Value::as_str);
            match delta_type {
                Some("text_delta") => {
                    if let Some(t) = delta.and_then(|d| d.get("text")).and_then(Value::as_str) {
                        emit_text_delta(text_stream, t);
                        state.text.push_str(t);
                    }
                }
                Some("thinking_delta") => {
                    if let Some(t) = delta
                        .and_then(|d| d.get("thinking"))
                        .and_then(Value::as_str)
                    {
                        state.reasoning.push_str(t);
                        if let Some(PartialBlock::Thinking { text }) = state.blocks.get_mut(&index)
                        {
                            text.push_str(t);
                        }
                    }
                }
                Some("signature_delta") => {
                    // Signature is verified by Anthropic clients; we keep accumulated thinking only.
                }
                Some("input_json_delta") => {
                    if let Some(partial) = delta
                        .and_then(|d| d.get("partial_json"))
                        .and_then(Value::as_str)
                    {
                        if let Some(PartialBlock::ToolUse { input_json, .. }) =
                            state.blocks.get_mut(&index)
                        {
                            input_json.push_str(partial);
                        }
                    }
                }
                _ => {}
            }
        }
        Some("content_block_stop") => {
            let index = payload.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
            if let Some(block) = state.blocks.remove(&index) {
                match block {
                    PartialBlock::ToolUse {
                        id,
                        name,
                        input_json,
                    } => {
                        let arguments: Value = if input_json.trim().is_empty() {
                            json!({})
                        } else {
                            serde_json::from_str(&input_json)
                                .unwrap_or_else(|_| Value::String(input_json))
                        };
                        state.tool_calls.push(ToolCallRequest {
                            id,
                            name,
                            arguments,
                        });
                    }
                    PartialBlock::Thinking { text } => {
                        if !text.trim().is_empty() {
                            state
                                .thinking_blocks
                                .push(json!({"type": "thinking", "thinking": text}));
                        }
                    }
                    PartialBlock::Text => {}
                }
            }
        }
        Some("message_delta") => {
            if let Some(delta) = payload.get("delta") {
                if let Some(reason) = delta.get("stop_reason").and_then(Value::as_str) {
                    state.finish_reason = map_stop_reason(reason);
                }
            }
            if let Some(usage) = payload.get("usage") {
                merge_usage(state, usage);
            }
        }
        Some("ping") | Some("message_stop") | None => {}
        _ => {}
    }

    Ok(())
}

fn merge_usage(state: &mut AnthropicStreamState, usage: &Value) {
    if let Some(i) = usage.get("input_tokens").and_then(Value::as_u64) {
        state.usage.prompt_tokens = state.usage.prompt_tokens.max(i as usize);
    }
    if let Some(o) = usage.get("output_tokens").and_then(Value::as_u64) {
        state.usage.completion_tokens = state.usage.completion_tokens.max(o as usize);
    }
    if let Some(c) = usage.get("cache_read_input_tokens").and_then(Value::as_u64) {
        state.usage.cached_prompt_tokens = state.usage.cached_prompt_tokens.max(c as usize);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn normalize_model_strips_anthropic_prefix() {
        assert_eq!(
            normalize_model_id("anthropic/claude-sonnet-4-20250514", DEFAULT_MODEL),
            "claude-sonnet-4-20250514"
        );
    }

    #[test]
    fn converts_tools_and_round_trips_tool_result() {
        let tools = vec![json!({
            "type": "function",
            "function": {
                "name": "get_weather",
                "description": "weather",
                "parameters": {"type": "object", "properties": {"loc": {"type": "string"}}}
            }
        })];
        let anth = convert_openai_tools_to_anthropic(&tools);
        assert_eq!(anth[0]["name"], "get_weather");
        assert!(anth[0].get("input_schema").is_some());

        let tool_msg = ChatMessage {
            role: "tool".to_string(),
            content: Some(json!("sunny")),
            tool_calls: None,
            tool_call_id: Some("toolu_1".to_string()),
            name: Some("get_weather".to_string()),
            timestamp: None,
            reasoning_content: None,
            thinking_blocks: None,
            metadata: None,
        };
        let b = tool_message_to_block(&tool_msg).unwrap();
        assert_eq!(b["type"], "tool_result");
        assert_eq!(b["tool_use_id"], "toolu_1");
    }

    #[test]
    fn parse_non_streaming_tool_use() {
        let v = json!({
            "content": [
                {"type": "text", "text": "I'll check."},
                {"type": "tool_use", "id": "toolu_01", "name": "x", "input": {"a": 1}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 10, "output_tokens": 20}
        });
        let r = parse_non_streaming_message(&v).unwrap();
        assert_eq!(r.tool_calls.len(), 1);
        assert_eq!(r.tool_calls[0].name, "x");
        assert_eq!(r.finish_reason, "tool_calls");
        assert_eq!(r.usage.prompt_tokens, 10);
        assert_eq!(r.usage.completion_tokens, 20);
    }
}
