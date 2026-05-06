use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::Result;
use async_trait::async_trait;
use reqwest::Client;
use serde::Serialize;
use serde_json::Value;
use sysinfo::System;
use tokio::process::Command;

use crate::providers::{
    GenerationSettings, LlmProvider, LlmResponse, ProviderModelInfo, SharedProvider,
    TextStreamCallback,
};
use crate::storage::ChatMessage;

#[derive(Debug, Clone, Serialize, Default)]
pub struct ProviderTelemetrySnapshot {
    pub provider_name: String,
    pub model: String,
    pub api_base: Option<String>,
    pub requests: u64,
    pub successes: u64,
    pub failures: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub last_prompt_tokens: u64,
    pub last_completion_tokens: u64,
    pub total_latency_ms: u64,
    pub avg_latency_ms: f64,
    pub avg_prefill_tokens_per_s: f64,
    pub avg_generation_tokens_per_s: f64,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct RuntimeTelemetrySnapshot {
    pub uptime_seconds: u64,
    pub inbound_messages: u64,
    pub outbound_messages: u64,
    pub provider: ProviderTelemetrySnapshot,
}

#[derive(Debug)]
struct TelemetryInner {
    started_at: Instant,
    inbound_messages: u64,
    outbound_messages: u64,
    provider: ProviderTelemetrySnapshot,
}

#[derive(Debug, Clone)]
pub struct RuntimeTelemetry {
    inner: Arc<Mutex<TelemetryInner>>,
}

impl RuntimeTelemetry {
    pub fn new(
        provider_name: impl Into<String>,
        model: impl Into<String>,
        api_base: Option<String>,
    ) -> Self {
        Self {
            inner: Arc::new(Mutex::new(TelemetryInner {
                started_at: Instant::now(),
                inbound_messages: 0,
                outbound_messages: 0,
                provider: ProviderTelemetrySnapshot {
                    provider_name: provider_name.into(),
                    model: model.into(),
                    api_base,
                    ..ProviderTelemetrySnapshot::default()
                },
            })),
        }
    }

    pub fn record_inbound(&self) {
        self.inner
            .lock()
            .expect("telemetry lock poisoned")
            .inbound_messages += 1;
    }

    pub fn record_outbound(&self) {
        self.inner
            .lock()
            .expect("telemetry lock poisoned")
            .outbound_messages += 1;
    }

    pub fn record_provider_success(&self, response: &LlmResponse, latency_ms: u64) {
        let mut inner = self.inner.lock().expect("telemetry lock poisoned");
        let provider = &mut inner.provider;
        provider.requests += 1;
        provider.successes += 1;
        provider.prompt_tokens += response.usage.prompt_tokens as u64;
        provider.completion_tokens += response.usage.completion_tokens as u64;
        provider.last_prompt_tokens = response.usage.prompt_tokens as u64;
        provider.last_completion_tokens = response.usage.completion_tokens as u64;
        provider.total_latency_ms += latency_ms;
        let successes = provider.successes.max(1) as f64;
        provider.avg_latency_ms = provider.total_latency_ms as f64 / successes;
        let total_seconds = (provider.total_latency_ms.max(1) as f64) / 1000.0;
        provider.avg_prefill_tokens_per_s = provider.prompt_tokens as f64 / total_seconds;
        provider.avg_generation_tokens_per_s = provider.completion_tokens as f64 / total_seconds;
        provider.last_error = None;
    }

    pub fn record_provider_failure(&self, error: &str, latency_ms: u64) {
        let mut inner = self.inner.lock().expect("telemetry lock poisoned");
        let provider = &mut inner.provider;
        provider.requests += 1;
        provider.failures += 1;
        provider.total_latency_ms += latency_ms;
        provider.last_error = Some(error.to_string());
        let attempts = provider.requests.max(1) as f64;
        provider.avg_latency_ms = provider.total_latency_ms as f64 / attempts;
    }

    pub fn snapshot(&self) -> RuntimeTelemetrySnapshot {
        let inner = self.inner.lock().expect("telemetry lock poisoned");
        RuntimeTelemetrySnapshot {
            uptime_seconds: inner.started_at.elapsed().as_secs(),
            inbound_messages: inner.inbound_messages,
            outbound_messages: inner.outbound_messages,
            provider: inner.provider.clone(),
        }
    }

    pub fn render_prometheus(&self) -> String {
        let snapshot = self.snapshot();
        let provider = &snapshot.provider;
        [
            "# HELP rbot_inbound_messages_total Inbound messages processed",
            "# TYPE rbot_inbound_messages_total counter",
            &format!("rbot_inbound_messages_total {}", snapshot.inbound_messages),
            "# HELP rbot_outbound_messages_total Outbound messages published",
            "# TYPE rbot_outbound_messages_total counter",
            &format!("rbot_outbound_messages_total {}", snapshot.outbound_messages),
            "# HELP rbot_provider_requests_total Provider requests issued",
            "# TYPE rbot_provider_requests_total counter",
            &format!(
                "rbot_provider_requests_total{{provider=\"{}\",model=\"{}\"}} {}",
                provider.provider_name, provider.model, provider.requests
            ),
            "# HELP rbot_provider_successes_total Successful provider requests",
            "# TYPE rbot_provider_successes_total counter",
            &format!(
                "rbot_provider_successes_total{{provider=\"{}\",model=\"{}\"}} {}",
                provider.provider_name, provider.model, provider.successes
            ),
            "# HELP rbot_provider_failures_total Failed provider requests",
            "# TYPE rbot_provider_failures_total counter",
            &format!(
                "rbot_provider_failures_total{{provider=\"{}\",model=\"{}\"}} {}",
                provider.provider_name, provider.model, provider.failures
            ),
            "# HELP rbot_prompt_tokens_total Prompt tokens sent to the provider",
            "# TYPE rbot_prompt_tokens_total counter",
            &format!(
                "rbot_prompt_tokens_total{{provider=\"{}\",model=\"{}\"}} {}",
                provider.provider_name, provider.model, provider.prompt_tokens
            ),
            "# HELP rbot_completion_tokens_total Completion tokens returned by the provider",
            "# TYPE rbot_completion_tokens_total counter",
            &format!(
                "rbot_completion_tokens_total{{provider=\"{}\",model=\"{}\"}} {}",
                provider.provider_name, provider.model, provider.completion_tokens
            ),
            "# HELP rbot_provider_avg_latency_ms Average provider request latency in ms",
            "# TYPE rbot_provider_avg_latency_ms gauge",
            &format!(
                "rbot_provider_avg_latency_ms{{provider=\"{}\",model=\"{}\"}} {:.2}",
                provider.provider_name, provider.model, provider.avg_latency_ms
            ),
            "# HELP rbot_provider_avg_prefill_tokens_per_second Average prompt throughput",
            "# TYPE rbot_provider_avg_prefill_tokens_per_second gauge",
            &format!(
                "rbot_provider_avg_prefill_tokens_per_second{{provider=\"{}\",model=\"{}\"}} {:.2}",
                provider.provider_name, provider.model, provider.avg_prefill_tokens_per_s
            ),
            "# HELP rbot_provider_avg_generation_tokens_per_second Average completion throughput",
            "# TYPE rbot_provider_avg_generation_tokens_per_second gauge",
            &format!(
                "rbot_provider_avg_generation_tokens_per_second{{provider=\"{}\",model=\"{}\"}} {:.2}",
                provider.provider_name, provider.model, provider.avg_generation_tokens_per_s
            ),
        ]
        .join("\n")
            + "\n"
    }
}

pub struct InstrumentedProvider {
    inner: SharedProvider,
    telemetry: RuntimeTelemetry,
}

impl InstrumentedProvider {
    pub fn new(inner: SharedProvider, telemetry: RuntimeTelemetry) -> Self {
        Self { inner, telemetry }
    }
}

#[async_trait]
impl LlmProvider for InstrumentedProvider {
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
        let started_at = Instant::now();
        match self
            .inner
            .chat(messages, tools, model, max_tokens, temperature)
            .await
        {
            Ok(response) => {
                self.telemetry
                    .record_provider_success(&response, started_at.elapsed().as_millis() as u64);
                Ok(response)
            }
            Err(err) => {
                self.telemetry.record_provider_failure(
                    &err.to_string(),
                    started_at.elapsed().as_millis() as u64,
                );
                Err(err)
            }
        }
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
        let started_at = Instant::now();
        match self
            .inner
            .chat_stream(messages, tools, model, max_tokens, temperature, text_stream)
            .await
        {
            Ok(response) => {
                self.telemetry
                    .record_provider_success(&response, started_at.elapsed().as_millis() as u64);
                Ok(response)
            }
            Err(err) => {
                self.telemetry.record_provider_failure(
                    &err.to_string(),
                    started_at.elapsed().as_millis() as u64,
                );
                Err(err)
            }
        }
    }

    async fn chat_with_retry(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Value]>,
        model: Option<&str>,
        max_tokens: Option<usize>,
        temperature: Option<f32>,
    ) -> Result<LlmResponse> {
        let started_at = Instant::now();
        match self
            .inner
            .chat_with_retry(messages, tools, model, max_tokens, temperature)
            .await
        {
            Ok(response) => {
                self.telemetry
                    .record_provider_success(&response, started_at.elapsed().as_millis() as u64);
                Ok(response)
            }
            Err(err) => {
                self.telemetry.record_provider_failure(
                    &err.to_string(),
                    started_at.elapsed().as_millis() as u64,
                );
                Err(err)
            }
        }
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
        let started_at = Instant::now();
        match self
            .inner
            .chat_with_retry_stream(messages, tools, model, max_tokens, temperature, text_stream)
            .await
        {
            Ok(response) => {
                self.telemetry
                    .record_provider_success(&response, started_at.elapsed().as_millis() as u64);
                Ok(response)
            }
            Err(err) => {
                self.telemetry.record_provider_failure(
                    &err.to_string(),
                    started_at.elapsed().as_millis() as u64,
                );
                Err(err)
            }
        }
    }

    async fn list_models(&self) -> Result<Vec<ProviderModelInfo>> {
        self.inner.list_models().await
    }
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct GpuSnapshot {
    pub name: String,
    pub utilization_pct: Option<f32>,
    pub memory_used_mb: Option<u64>,
    pub memory_total_mb: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct SystemSnapshot {
    pub host_name: Option<String>,
    pub os_name: Option<String>,
    pub kernel_version: Option<String>,
    pub total_memory_bytes: u64,
    pub used_memory_bytes: u64,
    pub total_swap_bytes: u64,
    pub used_swap_bytes: u64,
    pub cpu_usage_pct: f32,
    pub process_count: usize,
    pub gpus: Vec<GpuSnapshot>,
}

pub async fn collect_system_snapshot() -> SystemSnapshot {
    let mut system = System::new_all();
    system.refresh_all();
    SystemSnapshot {
        host_name: System::host_name(),
        os_name: System::long_os_version().or_else(System::name),
        kernel_version: System::kernel_version(),
        total_memory_bytes: system.total_memory(),
        used_memory_bytes: system.used_memory(),
        total_swap_bytes: system.total_swap(),
        used_swap_bytes: system.used_swap(),
        cpu_usage_pct: system.global_cpu_usage(),
        process_count: system.processes().len(),
        gpus: collect_gpu_snapshots().await,
    }
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct ProviderModelSnapshot {
    pub provider_name: String,
    pub api_base: Option<String>,
    pub model_name: String,
    pub model_id: String,
    pub model_path: Option<String>,
    pub model_size_bytes: Option<u64>,
    pub context_window_tokens: Option<usize>,
    pub available_models: Vec<String>,
    pub raw_details: BTreeMap<String, String>,
}

pub async fn collect_provider_model_snapshot(
    provider_name: &str,
    model: &str,
    api_base: Option<&str>,
) -> ProviderModelSnapshot {
    let mut snapshot = ProviderModelSnapshot {
        provider_name: provider_name.to_string(),
        api_base: api_base.map(ToOwned::to_owned),
        model_name: model.to_string(),
        model_id: model.to_string(),
        ..ProviderModelSnapshot::default()
    };
    let Some(api_base) = api_base else {
        return snapshot;
    };

    let client = match Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
    {
        Ok(client) => client,
        Err(_) => return snapshot,
    };

    if provider_name == "ollama" {
        collect_ollama_snapshot(&client, model, api_base, &mut snapshot).await;
    } else {
        collect_openai_compatible_snapshot(&client, api_base, &mut snapshot).await;
    }
    snapshot
}

async fn collect_ollama_snapshot(
    client: &Client,
    model: &str,
    api_base: &str,
    snapshot: &mut ProviderModelSnapshot,
) {
    let root = api_base.trim_end_matches("/v1").trim_end_matches('/');
    if let Ok(response) = client.get(format!("{root}/api/tags")).send().await {
        if let Ok(payload) = response.json::<Value>().await {
            if let Some(models) = payload.get("models").and_then(Value::as_array) {
                snapshot.available_models = models
                    .iter()
                    .filter_map(|item| item.get("name").and_then(Value::as_str))
                    .map(ToOwned::to_owned)
                    .collect();
                let target = strip_provider_prefix(model);
                if let Some(found) = models
                    .iter()
                    .find(|item| item.get("name").and_then(Value::as_str) == Some(target.as_str()))
                {
                    snapshot.model_id = found
                        .get("model")
                        .or_else(|| found.get("name"))
                        .and_then(Value::as_str)
                        .unwrap_or(target.as_str())
                        .to_string();
                    snapshot.model_size_bytes = found
                        .get("size")
                        .and_then(Value::as_u64)
                        .or(snapshot.model_size_bytes);
                    snapshot.context_window_tokens =
                        extract_context_window_tokens(found).or(snapshot.context_window_tokens);
                    if let Some(details) = found.get("details").and_then(Value::as_object) {
                        for (key, value) in details {
                            snapshot
                                .raw_details
                                .insert(key.clone(), json_value_to_string(value));
                        }
                    }
                }
            }
        }
    }

    if let Ok(response) = client
        .post(format!("{root}/api/show"))
        .json(&serde_json::json!({"name": strip_provider_prefix(model)}))
        .send()
        .await
    {
        if let Ok(payload) = response.json::<Value>().await {
            snapshot.context_window_tokens =
                extract_context_window_tokens(&payload).or(snapshot.context_window_tokens);
            snapshot.model_path = payload
                .get("modelfile")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            if let Some(details) = payload.get("details").and_then(Value::as_object) {
                for (key, value) in details {
                    snapshot
                        .raw_details
                        .insert(key.clone(), json_value_to_string(value));
                }
            }
            if let Some(info) = payload.get("model_info").and_then(Value::as_object) {
                for (key, value) in info {
                    snapshot
                        .raw_details
                        .entry(key.clone())
                        .or_insert_with(|| json_value_to_string(value));
                }
            }
        }
    }
}

async fn collect_openai_compatible_snapshot(
    client: &Client,
    api_base: &str,
    snapshot: &mut ProviderModelSnapshot,
) {
    if let Ok(response) = client
        .get(format!("{}/models", api_base.trim_end_matches('/')))
        .send()
        .await
    {
        if let Ok(payload) = response.json::<Value>().await {
            if let Some(models) = payload.get("data").and_then(Value::as_array) {
                snapshot.available_models = models
                    .iter()
                    .filter_map(|item| item.get("id").and_then(Value::as_str))
                    .map(ToOwned::to_owned)
                    .collect();
                if let Some(found) = models.iter().find(|item| {
                    item.get("id").and_then(Value::as_str) == Some(snapshot.model_name.as_str())
                }) {
                    snapshot.model_id = found
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or(snapshot.model_name.as_str())
                        .to_string();
                    snapshot.context_window_tokens =
                        extract_context_window_tokens(found).or(snapshot.context_window_tokens);
                    if let Some(root) = found.as_object() {
                        for (key, value) in root {
                            if key == "id" || key == "object" {
                                continue;
                            }
                            snapshot
                                .raw_details
                                .insert(key.clone(), json_value_to_string(value));
                        }
                    }
                }
            }
        }
    }
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

async fn collect_gpu_snapshots() -> Vec<GpuSnapshot> {
    let output = Command::new("nvidia-smi")
        .args([
            "--query-gpu=name,utilization.gpu,memory.used,memory.total",
            "--format=csv,noheader,nounits",
        ])
        .output()
        .await;
    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let parts = line.split(',').map(|part| part.trim()).collect::<Vec<_>>();
            if parts.len() != 4 {
                return None;
            }
            Some(GpuSnapshot {
                name: parts[0].to_string(),
                utilization_pct: parts[1].parse::<f32>().ok(),
                memory_used_mb: parts[2].parse::<u64>().ok(),
                memory_total_mb: parts[3].parse::<u64>().ok(),
            })
        })
        .collect()
}

fn strip_provider_prefix(model: &str) -> String {
    model
        .split_once('/')
        .map(|(_, suffix)| suffix.to_string())
        .unwrap_or_else(|| model.to_string())
}

fn json_value_to_string(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        _ => value.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::RuntimeTelemetry;
    use crate::providers::{LlmResponse, LlmUsage};

    #[test]
    fn telemetry_tracks_token_rates() {
        let telemetry = RuntimeTelemetry::new(
            "ollama",
            "ollama/qwen",
            Some("http://localhost".to_string()),
        );
        telemetry.record_inbound();
        telemetry.record_outbound();
        telemetry.record_provider_success(
            &LlmResponse {
                content: Some("ok".to_string()),
                tool_calls: Vec::new(),
                finish_reason: "stop".to_string(),
                usage: LlmUsage {
                    prompt_tokens: 120,
                    completion_tokens: 40,
                    ..Default::default()
                },
                reasoning_content: None,
                thinking_blocks: None,
            },
            2_000,
        );
        let snapshot = telemetry.snapshot();
        assert_eq!(snapshot.inbound_messages, 1);
        assert_eq!(snapshot.outbound_messages, 1);
        assert_eq!(snapshot.provider.requests, 1);
        assert!(snapshot.provider.avg_prefill_tokens_per_s >= 50.0);
        assert!(snapshot.provider.avg_generation_tokens_per_s >= 15.0);
    }
}
