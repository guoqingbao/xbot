use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::providers::registry::{PROVIDERS, find_by_name, normalize_provider_name};
use crate::util::ensure_dir;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentDefaults {
    pub workspace: String,
    pub model: String,
    pub provider: String,
    #[serde(alias = "maxTokens")]
    pub max_tokens: usize,
    #[serde(alias = "contextWindowTokens")]
    pub context_window_tokens: usize,
    pub temperature: f32,
    #[serde(alias = "maxToolIterations")]
    pub max_tool_iterations: usize,
    #[serde(alias = "maxConcurrentTools")]
    pub max_concurrent_tools: usize,
    #[serde(alias = "memoryMaxBytes")]
    pub memory_max_bytes: usize,
    #[serde(alias = "maxConcurrentRequests")]
    pub max_concurrent_requests: usize,
}

impl Default for AgentDefaults {
    fn default() -> Self {
        Self {
            workspace: "~/.rbot/workspace".to_string(),
            model: "openai/gpt-4.1-mini".to_string(),
            provider: "auto".to_string(),
            max_tokens: 8192,
            context_window_tokens: 65_536,
            temperature: 0.1,
            max_tool_iterations: 0,
            max_concurrent_tools: 5,
            memory_max_bytes: 32 * 1024,
            max_concurrent_requests: 3,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SubagentConfig {
    pub model: String,
    pub provider: String,
    #[serde(alias = "apiBase")]
    pub api_base: Option<String>,
}

impl Default for SubagentConfig {
    fn default() -> Self {
        Self {
            model: String::new(),
            provider: "auto".to_string(),
            api_base: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct AgentsConfig {
    pub defaults: AgentDefaults,
    pub subagents: SubagentConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ChannelsConfig {
    #[serde(alias = "sendProgress")]
    pub send_progress: bool,
    #[serde(alias = "sendToolHints")]
    pub send_tool_hints: bool,
    #[serde(alias = "transcriptionApiKey")]
    pub transcription_api_key: String,
    #[serde(alias = "sendMaxRetries")]
    pub send_max_retries: usize,
    #[serde(flatten)]
    pub sections: BTreeMap<String, serde_json::Value>,
}

impl Default for ChannelsConfig {
    fn default() -> Self {
        Self {
            send_progress: true,
            send_tool_hints: false,
            transcription_api_key: String::new(),
            send_max_retries: 3,
            sections: BTreeMap::new(),
        }
    }
}

impl ChannelsConfig {
    pub fn section(&self, name: &str) -> Option<&serde_json::Value> {
        self.sections.get(name)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ProviderConfig {
    #[serde(alias = "apiKey")]
    pub api_key: String,
    #[serde(alias = "apiBase")]
    pub api_base: Option<String>,
    #[serde(alias = "extraHeaders")]
    pub extra_headers: BTreeMap<String, String>,
    #[serde(alias = "reasoningEffort")]
    pub reasoning_effort: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct HeartbeatConfig {
    pub enabled: bool,
    #[serde(alias = "intervalS")]
    pub interval_s: u64,
}

impl Default for HeartbeatConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_s: 30 * 60,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AdminUiConfig {
    pub enabled: bool,
    pub path: String,
}

impl Default for AdminUiConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            path: "/admin".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MetricsConfig {
    pub enabled: bool,
    pub path: String,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            path: "/metrics".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GatewayConfig {
    pub host: String,
    pub port: u16,
    pub heartbeat: HeartbeatConfig,
    pub admin: AdminUiConfig,
    pub metrics: MetricsConfig,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            host: "0.0.0.0".to_string(),
            port: 18_790,
            heartbeat: HeartbeatConfig::default(),
            admin: AdminUiConfig::default(),
            metrics: MetricsConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WebSearchConfig {
    pub provider: String,
    #[serde(alias = "apiKey")]
    pub api_key: String,
    #[serde(alias = "baseUrl")]
    pub base_url: Option<String>,
    #[serde(alias = "maxResults")]
    pub max_results: usize,
}

impl Default for WebSearchConfig {
    fn default() -> Self {
        Self {
            provider: "duckduckgo".to_string(),
            api_key: String::new(),
            base_url: None,
            max_results: 5,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct WebToolsConfig {
    pub proxy: Option<String>,
    pub search: WebSearchConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ExecToolConfig {
    pub enable: bool,
    pub timeout: u64,
    #[serde(alias = "pathAppend")]
    pub path_append: String,
}

impl Default for ExecToolConfig {
    fn default() -> Self {
        Self {
            enable: true,
            timeout: 60,
            path_append: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct McpServerConfig {
    pub enabled: bool,
    #[serde(rename = "type")]
    pub transport: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub url: Option<String>,
    pub headers: BTreeMap<String, String>,
    #[serde(alias = "enabledTools")]
    pub enabled_tools: Vec<String>,
    #[serde(alias = "toolTimeout")]
    pub tool_timeout: u64,
}

impl Default for McpServerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            transport: "stdio".to_string(),
            command: String::new(),
            args: Vec::new(),
            env: BTreeMap::new(),
            url: None,
            headers: BTreeMap::new(),
            enabled_tools: vec!["*".to_string()],
            tool_timeout: 30,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolsConfig {
    pub web: WebToolsConfig,
    pub exec: ExecToolConfig,
    #[serde(alias = "mcpServers")]
    pub mcp_servers: BTreeMap<String, McpServerConfig>,
    #[serde(alias = "restrictToWorkspace")]
    pub restrict_to_workspace: bool,
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            web: WebToolsConfig::default(),
            exec: ExecToolConfig::default(),
            mcp_servers: BTreeMap::new(),
            restrict_to_workspace: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub agents: AgentsConfig,
    pub channels: ChannelsConfig,
    pub providers: BTreeMap<String, ProviderConfig>,
    pub gateway: GatewayConfig,
    pub tools: ToolsConfig,
}

impl Default for Config {
    fn default() -> Self {
        let mut providers = BTreeMap::new();
        providers.insert("openai".to_string(), ProviderConfig::default());
        Self {
            agents: AgentsConfig::default(),
            channels: ChannelsConfig::default(),
            providers,
            gateway: GatewayConfig::default(),
            tools: ToolsConfig::default(),
        }
    }
}

impl Config {
    pub fn default_path() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".rbot")
            .join("config.json")
    }

    pub fn load(path: Option<&Path>) -> Result<Self> {
        let path = path
            .map(Path::to_path_buf)
            .unwrap_or_else(Self::default_path);
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("failed to read config {}", path.display()))?;
        let value: serde_json::Value = serde_json::from_str(&raw)
            .with_context(|| format!("invalid JSON in {}", path.display()))?;
        let migrated = Self::migrate_value(value);
        let config = serde_json::from_value::<Self>(migrated)
            .with_context(|| format!("failed to parse config {}", path.display()))?;
        Ok(config)
    }

    pub fn save(&self, path: Option<&Path>) -> Result<PathBuf> {
        let path = path
            .map(Path::to_path_buf)
            .unwrap_or_else(Self::default_path);
        if let Some(parent) = path.parent() {
            ensure_dir(parent)?;
        }
        let providers = self
            .providers
            .iter()
            .map(|(name, cfg)| {
                (
                    name.clone(),
                    serde_json::json!({
                        "apiKey": cfg.api_key,
                        "apiBase": cfg.api_base,
                        "extraHeaders": cfg.extra_headers,
                        "reasoningEffort": cfg.reasoning_effort,
                    }),
                )
            })
            .collect::<serde_json::Map<String, serde_json::Value>>();
        let mut channels = self
            .channels
            .sections
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect::<serde_json::Map<String, serde_json::Value>>();
        channels.insert(
            "sendProgress".to_string(),
            serde_json::Value::Bool(self.channels.send_progress),
        );
        channels.insert(
            "sendToolHints".to_string(),
            serde_json::Value::Bool(self.channels.send_tool_hints),
        );
        channels.insert(
            "transcriptionApiKey".to_string(),
            serde_json::Value::String(self.channels.transcription_api_key.clone()),
        );
        let payload = serde_json::to_string_pretty(&serde_json::json!({
            "agents": {
                "defaults": {
                    "workspace": self.agents.defaults.workspace,
                    "model": self.agents.defaults.model,
                    "provider": self.agents.defaults.provider,
                    "maxTokens": self.agents.defaults.max_tokens,
                    "contextWindowTokens": self.agents.defaults.context_window_tokens,
                    "temperature": self.agents.defaults.temperature,
                    "maxToolIterations": self.agents.defaults.max_tool_iterations,
                    "maxConcurrentTools": self.agents.defaults.max_concurrent_tools,
                    "memoryMaxBytes": self.agents.defaults.memory_max_bytes,
                    "maxConcurrentRequests": self.agents.defaults.max_concurrent_requests,
                },
                "subagents": {
                    "model": self.agents.subagents.model,
                    "provider": self.agents.subagents.provider,
                    "apiBase": self.agents.subagents.api_base,
                }
            },
            "providers": providers,
            "channels": channels,
            "gateway": {
                "host": self.gateway.host,
                "port": self.gateway.port,
                "heartbeat": {
                    "enabled": self.gateway.heartbeat.enabled,
                    "intervalS": self.gateway.heartbeat.interval_s,
                },
                "admin": {
                    "enabled": self.gateway.admin.enabled,
                    "path": self.gateway.admin.path,
                },
                "metrics": {
                    "enabled": self.gateway.metrics.enabled,
                    "path": self.gateway.metrics.path,
                }
            },
            "tools": {
                "web": {
                    "proxy": self.tools.web.proxy,
                    "search": {
                        "provider": self.tools.web.search.provider,
                        "apiKey": self.tools.web.search.api_key,
                        "baseUrl": self.tools.web.search.base_url,
                        "maxResults": self.tools.web.search.max_results,
                    }
                },
                "exec": {
                    "enable": self.tools.exec.enable,
                    "timeout": self.tools.exec.timeout,
                    "pathAppend": self.tools.exec.path_append,
                },
                "mcpServers": self.tools.mcp_servers,
                "restrictToWorkspace": self.tools.restrict_to_workspace,
            }
        }))?;
        fs::write(&path, payload)?;
        Ok(path)
    }

    pub fn workspace_path(&self) -> PathBuf {
        expand_tilde(&self.agents.defaults.workspace)
    }

    pub fn provider_name_for_model(&self, model: Option<&str>) -> Option<String> {
        self.provider_name_for_model_with_forced(
            model,
            Some(self.agents.defaults.provider.as_str()),
        )
    }

    pub fn subagent_model(&self, main_model: &str) -> String {
        let model = self.agents.subagents.model.trim();
        if model.is_empty() {
            main_model.to_string()
        } else {
            model.to_string()
        }
    }

    pub fn subagent_provider_for_model(
        &self,
        model: Option<&str>,
    ) -> Option<(String, ProviderConfig)> {
        let name = self.provider_name_for_model_with_forced(
            model,
            Some(self.agents.subagents.provider.as_str()),
        )?;
        let cfg = self.providers.get(&name)?.clone();
        Some((name, cfg))
    }

    pub fn provider_api_base_for_provider(&self, provider_name: &str) -> Option<String> {
        let cfg = self.providers.get(provider_name)?;
        if let Some(api_base) = &cfg.api_base {
            return Some(api_base.clone());
        }
        find_by_name(provider_name).and_then(|spec| {
            (!spec.default_api_base.is_empty()).then(|| spec.default_api_base.to_string())
        })
    }

    fn provider_name_for_model_with_forced(
        &self,
        model: Option<&str>,
        forced_provider: Option<&str>,
    ) -> Option<String> {
        let forced = normalize_provider_name(forced_provider.unwrap_or("auto").trim());
        if forced != "auto" {
            return self.find_provider_key(&forced);
        }

        let model = model.unwrap_or(&self.agents.defaults.model).to_lowercase();
        let model_normalized = model.replace('-', "_");
        if let Some((prefix, _)) = model.split_once('/') {
            let normalized_prefix = normalize_provider_name(prefix);
            for spec in PROVIDERS {
                if normalize_provider_name(spec.name) != normalized_prefix {
                    continue;
                }
                if let Some(key) = self.find_provider_key(spec.name) {
                    let cfg = self.providers.get(&key)?;
                    if spec.is_oauth || spec.is_local || !cfg.api_key.is_empty() {
                        return Some(key);
                    }
                }
            }
        }

        for spec in PROVIDERS {
            if !spec
                .keywords
                .iter()
                .any(|kw| model.contains(kw) || model_normalized.contains(&kw.replace('-', "_")))
            {
                continue;
            }
            if let Some(key) = self.find_provider_key(spec.name) {
                let cfg = self.providers.get(&key)?;
                if spec.is_oauth || spec.is_local || !cfg.api_key.is_empty() {
                    return Some(key);
                }
            }
        }

        let mut local_fallback: Option<String> = None;
        for spec in PROVIDERS.iter().filter(|spec| spec.is_local) {
            if let Some(key) = self.find_provider_key(spec.name) {
                let cfg = self.providers.get(&key)?;
                if let Some(api_base) = &cfg.api_base {
                    if !spec.detect_by_base_keyword.is_empty()
                        && api_base.contains(spec.detect_by_base_keyword)
                    {
                        return Some(key);
                    }
                    if local_fallback.is_none() {
                        local_fallback = Some(key);
                    }
                }
            }
        }
        if local_fallback.is_some() {
            return local_fallback;
        }

        for spec in PROVIDERS.iter().filter(|spec| !spec.is_oauth) {
            if let Some(key) = self.find_provider_key(spec.name) {
                let cfg = self.providers.get(&key)?;
                if !cfg.api_key.is_empty() {
                    return Some(key);
                }
            }
        }
        None
    }

    pub fn provider_for_model(&self, model: Option<&str>) -> Option<(String, ProviderConfig)> {
        let name = self.provider_name_for_model(model)?;
        let cfg = self.providers.get(&name)?.clone();
        Some((name, cfg))
    }

    pub fn provider_api_base_for_model(&self, model: Option<&str>) -> Option<String> {
        let name = self.provider_name_for_model(model)?;
        let cfg = self.providers.get(&name)?;
        if let Some(api_base) = &cfg.api_base {
            return Some(api_base.clone());
        }
        find_by_name(&name).and_then(|spec| {
            (!spec.default_api_base.is_empty()).then(|| spec.default_api_base.to_string())
        })
    }

    fn migrate_value(mut value: serde_json::Value) -> serde_json::Value {
        if let Some(exec_cfg) = value
            .get_mut("tools")
            .and_then(|tools| tools.get_mut("exec"))
            .and_then(|exec| exec.as_object_mut())
        {
            if let Some(restrict) = exec_cfg.remove("restrictToWorkspace") {
                let tools = value
                    .get_mut("tools")
                    .and_then(|tools| tools.as_object_mut())
                    .expect("tools should be an object during migration");
                tools
                    .entry("restrictToWorkspace".to_string())
                    .or_insert(restrict);
            }
        }
        if let Some(defaults) = value
            .get_mut("agents")
            .and_then(|agents| agents.get_mut("defaults"))
            .and_then(|defaults| defaults.as_object_mut())
        {
            defaults.remove("memoryWindow");
        }
        if let Some(gateway) = value
            .get_mut("gateway")
            .and_then(|gateway| gateway.as_object_mut())
        {
            gateway
                .entry("admin".to_string())
                .or_insert_with(|| serde_json::json!({"enabled": true, "path": "/admin"}));
            gateway
                .entry("metrics".to_string())
                .or_insert_with(|| serde_json::json!({"enabled": true, "path": "/metrics"}));
        }
        value
    }

    fn find_provider_key(&self, expected: &str) -> Option<String> {
        let expected = normalize_provider_name(expected);
        self.providers
            .keys()
            .find(|key| normalize_provider_name(key) == expected)
            .cloned()
    }
}

pub fn expand_tilde(path: &str) -> PathBuf {
    if let Some(stripped) = path.strip_prefix("~/") {
        return dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(stripped);
    }
    PathBuf::from(path)
}
