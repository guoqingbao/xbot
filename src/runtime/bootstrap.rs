use std::collections::BTreeMap;
use std::net::IpAddr;

use anyhow::{Result, anyhow, bail};

use crate::channels::{
    DingTalkConfig, DiscordConfig, EmailConfig, FeishuConfig, MatrixConfig, MochatConfig, QqConfig,
    SlackConfig, TelegramConfig, WecomConfig, WhatsAppConfig,
};
use crate::config::{Config, ProviderConfig};
use crate::providers::registry::find_by_name;
use crate::providers::{
    AnthropicProvider, AzureOpenAiProvider, CustomProvider, GenerationSettings,
    OpenAiCompatibleProvider, SharedProvider,
};
use url::Url;

fn normalize_path(path: &str) -> String {
    if path.trim().is_empty() {
        "/".to_string()
    } else if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    }
}

pub fn validate_run_config(config: &Config, model: &str) -> Result<()> {
    if config.provider_for_model(Some(model)).is_none() {
        bail!("no configured provider matched model '{model}'");
    }
    let subagent_model = config.subagent_model(model);
    if (!config.agents.subagents.model.trim().is_empty()
        || crate::providers::registry::normalize_provider_name(
            config.agents.subagents.provider.trim(),
        ) != "auto"
        || config.agents.subagents.api_base.is_some())
        && config
            .subagent_provider_for_model(Some(&subagent_model))
            .is_none()
    {
        bail!("no configured provider matched subagent model '{subagent_model}'");
    }

    let mut webhook_paths = BTreeMap::<String, String>::new();

    if let Some(section) = config.channels.section("email") {
        let email: EmailConfig = serde_json::from_value(section.clone())
            .map_err(|err| anyhow!("invalid email channel config: {err}"))?;
        if email.enabled {
            if !email.consent_granted {
                bail!("email channel is enabled but consentGranted is false");
            }
            for (value, field) in [
                (&email.imap_host, "imapHost"),
                (&email.imap_username, "imapUsername"),
                (&email.imap_password, "imapPassword"),
                (&email.smtp_host, "smtpHost"),
                (&email.smtp_username, "smtpUsername"),
                (&email.smtp_password, "smtpPassword"),
            ] {
                if value.trim().is_empty() {
                    bail!("email channel is enabled but {field} is empty");
                }
            }
        }
    }

    if let Some(section) = config.channels.section("slack") {
        let slack: SlackConfig = serde_json::from_value(section.clone())
            .map_err(|err| anyhow!("invalid slack channel config: {err}"))?;
        if slack.enabled {
            if slack.bot_token.trim().is_empty() {
                bail!("slack channel is enabled but botToken is empty");
            }
            if slack.mode.eq_ignore_ascii_case("socket") {
                if slack.app_token.trim().is_empty() {
                    bail!("slack channel mode=socket but appToken is empty");
                }
            } else {
                if slack.signing_secret.trim().is_empty() {
                    bail!("slack channel is enabled but signingSecret is empty");
                }
                let path = normalize_path(&slack.webhook_path);
                if let Some(existing) = webhook_paths.insert(path.clone(), "slack".to_string()) {
                    bail!("duplicate webhook path '{path}' configured for {existing} and slack");
                }
            }
        }
    }

    if let Some(section) = config.channels.section("telegram") {
        let telegram: TelegramConfig = serde_json::from_value(section.clone())
            .map_err(|err| anyhow!("invalid telegram channel config: {err}"))?;
        if telegram.enabled {
            if telegram.token.trim().is_empty() {
                bail!("telegram channel is enabled but token is empty");
            }
            let path = normalize_path(&telegram.webhook_path);
            if let Some(existing) = webhook_paths.insert(path.clone(), "telegram".to_string()) {
                bail!("duplicate webhook path '{path}' configured for {existing} and telegram");
            }
        }
    }

    if let Some(section) = config.channels.section("feishu") {
        let feishu: FeishuConfig = serde_json::from_value(section.clone())
            .map_err(|err| anyhow!("invalid feishu channel config: {err}"))?;
        if feishu.enabled {
            if feishu.app_id.trim().is_empty() || feishu.app_secret.trim().is_empty() {
                bail!("feishu channel is enabled but appId/appSecret is incomplete");
            }
            let path = normalize_path(&feishu.webhook_path);
            if let Some(existing) = webhook_paths.insert(path.clone(), "feishu".to_string()) {
                bail!("duplicate webhook path '{path}' configured for {existing} and feishu");
            }
        }
    }

    if let Some(section) = config.channels.section("dingtalk") {
        let dt: DingTalkConfig = serde_json::from_value(section.clone())
            .map_err(|err| anyhow!("invalid dingtalk channel config: {err}"))?;
        if dt.enabled {
            if dt.app_key.trim().is_empty() || dt.app_secret.trim().is_empty() {
                bail!("dingtalk channel is enabled but appKey/appSecret is incomplete");
            }
            if dt.robot_code.trim().is_empty() {
                bail!("dingtalk channel is enabled but robotCode is empty (required for sending)");
            }
        }
    }

    if let Some(section) = config.channels.section("discord") {
        let dc: DiscordConfig = serde_json::from_value(section.clone())
            .map_err(|err| anyhow!("invalid discord channel config: {err}"))?;
        if dc.enabled && dc.bot_token.trim().is_empty() {
            bail!("discord channel is enabled but botToken is empty");
        }
    }

    if let Some(section) = config.channels.section("matrix") {
        let mx: MatrixConfig = serde_json::from_value(section.clone())
            .map_err(|err| anyhow!("invalid matrix channel config: {err}"))?;
        if mx.enabled {
            if mx.homeserver_url.trim().is_empty() {
                bail!("matrix channel is enabled but homeserverUrl is empty");
            }
            if mx.access_token.trim().is_empty() {
                bail!("matrix channel is enabled but accessToken is empty");
            }
        }
    }

    if let Some(section) = config.channels.section("whatsapp") {
        let wa: WhatsAppConfig = serde_json::from_value(section.clone())
            .map_err(|err| anyhow!("invalid whatsapp channel config: {err}"))?;
        if wa.enabled && wa.bridge_url.trim().is_empty() {
            bail!("whatsapp channel is enabled but bridgeUrl is empty");
        }
    }

    if let Some(section) = config.channels.section("qq") {
        let qq: QqConfig = serde_json::from_value(section.clone())
            .map_err(|err| anyhow!("invalid qq channel config: {err}"))?;
        if qq.enabled {
            if qq.app_id.trim().is_empty() || qq.secret.trim().is_empty() {
                bail!("qq channel is enabled but appId/secret is incomplete");
            }
        }
    }

    if let Some(section) = config.channels.section("wecom") {
        let wc: WecomConfig = serde_json::from_value(section.clone())
            .map_err(|err| anyhow!("invalid wecom channel config: {err}"))?;
        if wc.enabled {
            if wc.agent_id.trim().is_empty() || wc.secret.trim().is_empty() {
                bail!("wecom channel is enabled but agentId/secret is incomplete");
            }
            if wc.corp_id.trim().is_empty() {
                bail!("wecom channel is enabled but corpId is empty (required for sending)");
            }
        }
    }

    if let Some(section) = config.channels.section("mochat") {
        let mc: MochatConfig = serde_json::from_value(section.clone())
            .map_err(|err| anyhow!("invalid mochat channel config: {err}"))?;
        if mc.enabled && mc.claw_token.trim().is_empty() {
            bail!("mochat channel is enabled but clawToken is empty");
        }
    }

    for (name, server) in &config.tools.mcp_servers {
        if !server.enabled {
            continue;
        }
        let transport = if server.transport.trim().is_empty() {
            "stdio"
        } else {
            server.transport.as_str()
        };
        if transport != "stdio" && transport != "http" && transport != "streamableHttp" && transport != "sse" {
            bail!(
                "MCP server '{name}' uses unsupported transport '{transport}'; only stdio, http, streamableHttp, and sse are supported"
            );
        }
        if transport == "stdio" && server.command.trim().is_empty() {
            bail!("MCP server '{name}' is enabled but command is empty");
        }
        if (transport == "http" || transport == "streamableHttp" || transport == "sse") && server.url.is_none() {
            bail!("MCP server '{name}' uses HTTP transport but no URL is configured");
        }
    }

    Ok(())
}

pub fn build_provider_client(
    provider_name: &str,
    provider_cfg: &ProviderConfig,
    model: &str,
    resolved_api_base: Option<String>,
    proxy: Option<&str>,
    temperature: Option<f32>,
) -> Result<SharedProvider> {
    let spec = find_by_name(provider_name);
    let api_base = resolved_api_base
        .or_else(|| provider_cfg.api_base.clone())
        .or_else(|| {
            spec.and_then(|spec| {
                (!spec.default_api_base.is_empty()).then(|| spec.default_api_base.to_string())
            })
        })
        .or_else(|| (provider_name == "openai").then(|| "https://api.openai.com/v1".to_string()));
    if api_base.is_none() && provider_name != "custom" && !spec.map(|s| s.is_local).unwrap_or(false)
    {
        if let Some(s) = spec {
            if !s.is_oauth && s.default_api_base.is_empty() {
                bail!(
                    "provider '{provider_name}' requires apiBase because it has no built-in endpoint"
                );
            }
        }
    }
    let is_oauth = spec.map(|spec| spec.is_oauth).unwrap_or(false);
    let requires_api_key = !is_oauth
        && !spec.map(|spec| spec.is_local).unwrap_or(false)
        && !api_base
            .as_deref()
            .map(api_base_looks_local)
            .unwrap_or(false);
    if requires_api_key && provider_cfg.api_key.trim().is_empty() {
        bail!("provider '{provider_name}' is configured without an API key");
    }
    let generation = GenerationSettings {
        temperature,
        max_tokens: 16_384,
    };

    if provider_name == "custom" || spec.is_none() {
        return Ok(std::sync::Arc::new(CustomProvider::new(
            provider_cfg.api_key.clone(),
            api_base,
            model.to_string(),
            provider_cfg.extra_headers.clone(),
            generation,
            proxy,
        )?));
    }

    match provider_name {
        "azure_openai" => Ok(std::sync::Arc::new(AzureOpenAiProvider::new(
            provider_cfg.api_key.clone(),
            api_base.ok_or_else(|| anyhow!("provider 'azure_openai' requires api_base"))?,
            model.to_string(),
            generation,
            proxy,
        )?)),
        "anthropic" => Ok(std::sync::Arc::new(AnthropicProvider::new(
            provider_cfg.api_key.clone(),
            api_base,
            model.to_string(),
            provider_cfg.extra_headers.clone(),
            generation,
            proxy,
            provider_cfg.reasoning_effort.clone(),
        )?)),
        _ => Ok(std::sync::Arc::new(
            OpenAiCompatibleProvider::with_reasoning(
                provider_cfg.api_key.clone(),
                api_base,
                model.to_string(),
                provider_cfg.extra_headers.clone(),
                generation,
                proxy,
                provider_cfg.reasoning_effort.clone(),
            )?,
        )),
    }
}

fn api_base_looks_local(api_base: &str) -> bool {
    let Ok(url) = Url::parse(api_base) else {
        return false;
    };
    let Some(host) = url.host_str() else {
        return false;
    };
    if matches!(host, "localhost" | "0.0.0.0") || host.ends_with(".local") {
        return true;
    }
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) => ip.is_loopback() || ip.is_private(),
        Ok(IpAddr::V6(ip)) => ip.is_loopback() || ip.is_unique_local(),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::{build_provider_client, validate_run_config};
    use crate::config::{Config, ProviderConfig};
    use serde_json::json;

    #[test]
    fn validate_run_config_rejects_missing_slack_secret() {
        let config: Config = serde_json::from_value(json!({
            "agents": {
                "defaults": {
                    "model": "openai/gpt-4.1-mini",
                    "provider": "openai"
                }
            },
            "providers": {
                "openai": {
                    "apiKey": "sk-test"
                }
            },
            "channels": {
                "slack": {
                    "enabled": true,
                    "allowFrom": ["*"],
                    "botToken": "xoxb-test"
                }
            }
        }))
        .unwrap();

        let err = validate_run_config(&config, "openai/gpt-4.1-mini").unwrap_err();
        assert!(err.to_string().contains("signingSecret"));
    }

    #[test]
    fn validate_run_config_rejects_duplicate_webhook_paths() {
        let config: Config = serde_json::from_value(json!({
            "agents": {
                "defaults": {
                    "model": "openai/gpt-4.1-mini",
                    "provider": "openai"
                }
            },
            "providers": {
                "openai": {
                    "apiKey": "sk-test"
                }
            },
            "channels": {
                "telegram": {
                    "enabled": true,
                    "allowFrom": ["*"],
                    "token": "123:abc",
                    "webhookPath": "/events"
                },
                "feishu": {
                    "enabled": true,
                    "allowFrom": ["*"],
                    "appId": "cli_a",
                    "appSecret": "secret",
                    "webhookPath": "/events"
                }
            }
        }))
        .unwrap();

        let err = validate_run_config(&config, "openai/gpt-4.1-mini").unwrap_err();
        assert!(err.to_string().contains("duplicate webhook path"));
    }

    #[test]
    fn validate_run_config_accepts_email_and_webhook_channels() {
        let config: Config = serde_json::from_value(json!({
            "agents": {
                "defaults": {
                    "model": "openai/gpt-4.1-mini",
                    "provider": "openai"
                }
            },
            "providers": {
                "openai": {
                    "apiKey": "sk-test"
                }
            },
            "channels": {
                "email": {
                    "enabled": true,
                    "allowFrom": ["*"],
                    "consentGranted": true,
                    "imapHost": "imap.example.com",
                    "imapUsername": "bot@example.com",
                    "imapPassword": "imap-secret",
                    "smtpHost": "smtp.example.com",
                    "smtpUsername": "bot@example.com",
                    "smtpPassword": "smtp-secret"
                },
                "telegram": {
                    "enabled": true,
                    "allowFrom": ["*"],
                    "token": "123:abc",
                    "webhookPath": "/telegram/webhook"
                }
            }
        }))
        .unwrap();

        validate_run_config(&config, "openai/gpt-4.1-mini").unwrap();
    }

    #[test]
    fn build_provider_client_accepts_local_provider_without_api_key() {
        let provider = build_provider_client(
            "ollama",
            &ProviderConfig {
                api_key: String::new(),
                api_base: Some("http://localhost:11434/v1".to_string()),
                extra_headers: Default::default(),
                reasoning_effort: None,
            },
            "ollama/qwen2.5-coder:7b",
            Some("http://localhost:11434/v1".to_string()),
            None,
            None,
        )
        .unwrap();

        assert_eq!(provider.default_model(), "ollama/qwen2.5-coder:7b");
    }

    #[test]
    fn build_provider_client_accepts_private_api_base_without_api_key() {
        let provider = build_provider_client(
            "custom",
            &ProviderConfig {
                api_key: String::new(),
                api_base: Some("http://192.168.1.3:8000/v1".to_string()),
                extra_headers: Default::default(),
                reasoning_effort: None,
            },
            "Qwen3_5ForConditionalGeneration",
            Some("http://192.168.1.3:8000/v1".to_string()),
            None,
            None,
        )
        .unwrap();

        assert_eq!(provider.default_model(), "Qwen3_5ForConditionalGeneration");
    }

    #[test]
    fn validate_run_config_rejects_invalid_mcp_server() {
        let config: Config = serde_json::from_value(json!({
            "agents": {
                "defaults": {
                    "model": "openai/gpt-4.1-mini",
                    "provider": "openai"
                }
            },
            "providers": {
                "openai": {
                    "apiKey": "sk-test"
                }
            },
            "channels": {
                "email": {
                    "enabled": true,
                    "allowFrom": ["*"],
                    "consentGranted": true,
                    "imapHost": "imap.example.com",
                    "imapUsername": "bot@example.com",
                    "imapPassword": "imap-secret",
                    "smtpHost": "smtp.example.com",
                    "smtpUsername": "bot@example.com",
                    "smtpPassword": "smtp-secret"
                }
            },
            "tools": {
                "mcpServers": {
                    "github": {
                        "enabled": true,
                        "type": "grpc",
                        "url": "http://localhost:8001/mcp"
                    }
                }
            }
        }))
        .unwrap();

        let err = validate_run_config(&config, "openai/gpt-4.1-mini").unwrap_err();
        assert!(
            err.to_string()
                .contains("unsupported transport")
        );
    }
}
