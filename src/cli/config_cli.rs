use std::path::Path;

use anyhow::{Result, anyhow};
use console::{Style, Term};
use inquire::validator::Validation;
use inquire::{Confirm, Password, Select, Text};
use serde_json::Value;

use rbot::channels::discover_all;
use rbot::config::{Config, SubagentConfig};
use rbot::providers::registry::{PROVIDERS, find_by_name};

pub async fn run_config_provider(config_path: Option<&Path>) -> Result<()> {
    let mut config = Config::load(config_path)?;
    let _term = Term::stdout();
    let theme = inquire::ui::RenderConfig::default();

    println!(
        "{}",
        Style::new()
            .cyan()
            .bold()
            .apply_to("─ Provider Configuration")
    );

    let provider_names: Vec<String> = PROVIDERS.iter().map(|p| p.name.to_string()).collect();
    let selected_provider_name = Select::new("Select provider to configure:", provider_names)
        .with_render_config(theme)
        .prompt()?;

    let spec =
        find_by_name(&selected_provider_name).ok_or_else(|| anyhow!("Provider not found"))?;

    let mut provider_key = selected_provider_name.clone();
    if selected_provider_name == "custom" {
        provider_key = Text::new("Enter a unique name for this custom provider:").prompt()?;
    }

    let mut provider_cfg = config
        .providers
        .get(&provider_key)
        .cloned()
        .unwrap_or_default();

    if !spec.is_local && selected_provider_name != "custom" {
        if !spec.is_oauth {
            let new_api_key = Password::new("Enter API Key (leave blank to keep current):")
                .without_confirmation()
                .prompt()?;
            if !new_api_key.is_empty() {
                provider_cfg.api_key = new_api_key;
            }
        }

        if spec.default_api_base.is_empty() {
            provider_cfg.api_base = Some(
                Text::new("Enter API Base URL:")
                    .with_default(provider_cfg.api_base.as_deref().unwrap_or(""))
                    .with_validator(|value: &str| {
                        if value.trim().is_empty() {
                            Ok(Validation::Invalid("Cannot be empty".into()))
                        } else {
                            Ok(Validation::Valid)
                        }
                    })
                    .prompt()?,
            );
        }

        println!(
            "{}",
            Style::new().dim().apply_to("Fetching available models...")
        );
        let api_base = provider_cfg.api_base.clone().or_else(|| {
            if !spec.default_api_base.is_empty() {
                Some(spec.default_api_base.to_string())
            } else {
                None
            }
        });
        let api_key_opt = if provider_cfg.api_key.trim().is_empty() {
            None
        } else {
            Some(provider_cfg.api_key.as_str())
        };

        let snapshot = rbot::observability::collect_provider_model_snapshot(
            &selected_provider_name,
            &config.agents.defaults.model,
            api_base.as_deref(),
            api_key_opt,
        )
        .await;

        if !snapshot.available_models.is_empty() {
            let selected_model =
                Select::new("Select default model:", snapshot.available_models.clone()).prompt()?;
            config.agents.defaults.model = selected_model;
        } else {
            println!(
                "{}",
                Style::new()
                    .yellow()
                    .apply_to("Could not fetch models. Please enter model name manually.")
            );
            config.agents.defaults.model = Text::new("Default model:")
                .with_default(&config.agents.defaults.model)
                .prompt()?;
        }
    } else {
        // Custom or Local
        let hint = if selected_provider_name == "ollama" {
            "(e.g. http://localhost:11434/v1)"
        } else if selected_provider_name == "vllm" {
            "(e.g. http://localhost:8000/v1)"
        } else {
            "(e.g. https://api.yourprovider.com/v1)"
        };

        provider_cfg.api_base = Some(
            Text::new(&format!("Enter API Base URL {}:", hint))
                .with_default(
                    provider_cfg
                        .api_base
                        .as_deref()
                        .unwrap_or(spec.default_api_base),
                )
                .prompt()?,
        );

        if Confirm::new("Enter API Key? (Optional for local/custom)")
            .with_default(false)
            .prompt()?
        {
            provider_cfg.api_key = Text::new("Enter API Key:").prompt()?;
        }

        config.agents.defaults.model = Text::new("Default model:")
            .with_default(&config.agents.defaults.model)
            .prompt()?;
    }

    config.providers.insert(provider_key.clone(), provider_cfg);
    config.agents.defaults.provider = provider_key.clone();

    if Confirm::new("Configure subagent provider/model now?")
        .with_default(false)
        .prompt()?
    {
        configure_subagent_provider(&mut config, &provider_key).await?;
    }

    let path = config.save(config_path)?;
    println!(
        "\n{}",
        Style::new()
            .green()
            .bold()
            .apply_to("Configuration saved successfully!")
    );
    println!("Config file: {}", path.display());

    // Print final config
    println!(
        "\n{}",
        Style::new().cyan().apply_to("Final Provider Config:")
    );
    println!(
        "{}",
        serde_json::to_string_pretty(&config.providers.get(&config.agents.defaults.provider))?
    );

    Ok(())
}

async fn configure_subagent_provider(config: &mut Config, main_provider_key: &str) -> Result<()> {
    let mode = Select::new(
        "Subagent provider mode:",
        vec![
            "Inherit main provider/model".to_string(),
            "Use main provider with a different model".to_string(),
            "Use a separate provider/API base".to_string(),
        ],
    )
    .prompt()?;

    match mode.as_str() {
        "Inherit main provider/model" => {
            config.agents.subagents = SubagentConfig::default();
        }
        "Use main provider with a different model" => {
            config.agents.subagents.provider = main_provider_key.to_string();
            config.agents.subagents.api_base = None;
            config.agents.subagents.model = Text::new("Subagent model:")
                .with_default(if config.agents.subagents.model.trim().is_empty() {
                    &config.agents.defaults.model
                } else {
                    &config.agents.subagents.model
                })
                .prompt()?;
        }
        "Use a separate provider/API base" => {
            let (provider_key, model) = prompt_provider_entry(
                config,
                "Select subagent provider to configure:",
                "Subagent model:",
            )
            .await?;
            let api_base = config
                .providers
                .get(&provider_key)
                .and_then(|cfg| cfg.api_base.clone());
            config.agents.subagents.provider = provider_key;
            config.agents.subagents.model = model;
            config.agents.subagents.api_base = api_base;
        }
        _ => {}
    }
    Ok(())
}

async fn prompt_provider_entry(
    config: &mut Config,
    provider_prompt: &str,
    model_prompt: &str,
) -> Result<(String, String)> {
    let provider_names: Vec<String> = PROVIDERS.iter().map(|p| p.name.to_string()).collect();
    let selected_provider_name = Select::new(provider_prompt, provider_names).prompt()?;
    let spec =
        find_by_name(&selected_provider_name).ok_or_else(|| anyhow!("Provider not found"))?;

    let mut provider_key = selected_provider_name.clone();
    if selected_provider_name == "custom" {
        provider_key = Text::new("Enter a unique name for this custom provider:").prompt()?;
    }

    let mut provider_cfg = config
        .providers
        .get(&provider_key)
        .cloned()
        .unwrap_or_default();

    let model = if !spec.is_local && selected_provider_name != "custom" {
        if !spec.is_oauth {
            let new_api_key = Password::new("Enter API Key (leave blank to keep current):")
                .without_confirmation()
                .prompt()?;
            if !new_api_key.is_empty() {
                provider_cfg.api_key = new_api_key;
            }
        }

        if spec.default_api_base.is_empty() {
            provider_cfg.api_base = Some(
                Text::new("Enter API Base URL:")
                    .with_default(provider_cfg.api_base.as_deref().unwrap_or(""))
                    .with_validator(|value: &str| {
                        if value.trim().is_empty() {
                            Ok(Validation::Invalid("Cannot be empty".into()))
                        } else {
                            Ok(Validation::Valid)
                        }
                    })
                    .prompt()?,
            );
        }

        let api_base = provider_cfg.api_base.clone().or_else(|| {
            if !spec.default_api_base.is_empty() {
                Some(spec.default_api_base.to_string())
            } else {
                None
            }
        });
        let api_key_opt = if provider_cfg.api_key.trim().is_empty() {
            None
        } else {
            Some(provider_cfg.api_key.as_str())
        };

        println!(
            "{}",
            Style::new().dim().apply_to("Fetching available models...")
        );
        let snapshot = rbot::observability::collect_provider_model_snapshot(
            &selected_provider_name,
            &config.agents.defaults.model,
            api_base.as_deref(),
            api_key_opt,
        )
        .await;

        if !snapshot.available_models.is_empty() {
            Select::new(model_prompt, snapshot.available_models).prompt()?
        } else {
            Text::new(model_prompt)
                .with_default(&config.agents.defaults.model)
                .prompt()?
        }
    } else {
        let hint = if selected_provider_name == "ollama" {
            "(e.g. http://localhost:11434/v1)"
        } else if selected_provider_name == "vllm" {
            "(e.g. http://localhost:8000/v1)"
        } else {
            "(e.g. https://api.yourprovider.com/v1)"
        };

        provider_cfg.api_base = Some(
            Text::new(&format!("Enter API Base URL {}:", hint))
                .with_default(
                    provider_cfg
                        .api_base
                        .as_deref()
                        .unwrap_or(spec.default_api_base),
                )
                .prompt()?,
        );

        if Confirm::new("Enter API Key? (Optional for local/custom)")
            .with_default(false)
            .prompt()?
        {
            provider_cfg.api_key = Text::new("Enter API Key:").prompt()?;
        }

        Text::new(model_prompt)
            .with_default(&config.agents.defaults.model)
            .prompt()?
    };

    config.providers.insert(provider_key.clone(), provider_cfg);
    Ok((provider_key, model))
}

pub async fn run_config_channel(config_path: Option<&Path>) -> Result<()> {
    let mut config = Config::load(config_path)?;

    println!(
        "{}",
        Style::new()
            .magenta()
            .bold()
            .apply_to("─ Channel Configuration")
    );

    // Global channel settings
    config.channels.send_progress = Confirm::new("Send progress updates to channels?")
        .with_default(config.channels.send_progress)
        .prompt()?;

    config.channels.send_tool_hints = Confirm::new("Send tool execution hints to channels?")
        .with_default(config.channels.send_tool_hints)
        .prompt()?;

    let channels = discover_all();
    let channel_names: Vec<String> = channels.keys().cloned().collect();
    let selected_channel_name: String =
        Select::new("Select channel to configure:", channel_names).prompt()?;

    let descriptor = channels
        .get(&selected_channel_name)
        .ok_or_else(|| anyhow!("Channel not found"))?;
    let mut channel_config = config
        .channels
        .sections
        .get(&selected_channel_name)
        .cloned()
        .unwrap_or_else(|| descriptor.default_config.clone());

    if let Some(obj) = channel_config.as_object_mut() {
        if selected_channel_name == "slack" {
            obj.insert(
                "enabled".to_string(),
                Value::Bool(
                    Confirm::new("Enabled?")
                        .with_default(obj.get("enabled").and_then(Value::as_bool).unwrap_or(true))
                        .prompt()?,
                ),
            );

            obj.insert(
                "mode".to_string(),
                Value::String(
                    Select::new("Mode:", vec!["socket".to_string(), "webhook".to_string()])
                        .with_starting_cursor(
                            if obj.get("mode").and_then(Value::as_str) == Some("webhook") {
                                1
                            } else {
                                0
                            },
                        )
                        .prompt()?,
                ),
            );

            let current_allow = obj
                .get("allowFrom")
                .or_else(|| obj.get("allow_from"))
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str())
                        .collect::<Vec<_>>()
                        .join(",")
                })
                .unwrap_or_else(|| "*".to_string());
            let new_allow = Text::new("Allowed from (comma separated IDs or *):")
                .with_default(&current_allow)
                .prompt()?;
            obj.insert(
                "allowFrom".to_string(),
                Value::Array(
                    new_allow
                        .split(',')
                        .map(|s| Value::String(s.trim().to_string()))
                        .collect(),
                ),
            );
            obj.remove("allow_from");

            let bot_token = Text::new("Bot Token:")
                .with_placeholder("xoxb-...")
                .with_default(
                    obj.get("botToken")
                        .or_else(|| obj.get("bot_token"))
                        .and_then(Value::as_str)
                        .unwrap_or(""),
                )
                .with_validator(|s: &str| {
                    if s.trim().is_empty() {
                        Ok(Validation::Invalid("Cannot be empty".into()))
                    } else {
                        Ok(Validation::Valid)
                    }
                })
                .prompt()?;
            obj.insert("botToken".to_string(), Value::String(bot_token));

            let app_token = Text::new("App Token:")
                .with_placeholder("xapp-1-...")
                .with_default(
                    obj.get("appToken")
                        .or_else(|| obj.get("app_token"))
                        .and_then(Value::as_str)
                        .unwrap_or(""),
                )
                .with_validator(|s: &str| {
                    if s.trim().is_empty() {
                        Ok(Validation::Invalid("Cannot be empty".into()))
                    } else {
                        Ok(Validation::Valid)
                    }
                })
                .prompt()?;
            obj.insert("appToken".to_string(), Value::String(app_token));

            obj.insert(
                "replyInThread".to_string(),
                Value::Bool(
                    Confirm::new("Reply in thread?")
                        .with_default(
                            obj.get("replyInThread")
                                .or_else(|| obj.get("reply_in_thread"))
                                .and_then(Value::as_bool)
                                .unwrap_or(true),
                        )
                        .prompt()?,
                ),
            );

            obj.insert(
                "groupPolicy".to_string(),
                Value::String(
                    Select::new(
                        "Group Policy:",
                        vec!["mention".to_string(), "always".to_string()],
                    )
                    .with_starting_cursor(
                        if obj.get("groupPolicy").and_then(Value::as_str) == Some("always") {
                            1
                        } else {
                            0
                        },
                    )
                    .prompt()?,
                ),
            );
        } else if selected_channel_name == "feishu" {
            obj.insert(
                "enabled".to_string(),
                Value::Bool(
                    Confirm::new("Enabled?")
                        .with_default(obj.get("enabled").and_then(Value::as_bool).unwrap_or(true))
                        .prompt()?,
                ),
            );

            let current_allow = obj
                .get("allowFrom")
                .or_else(|| obj.get("allow_from"))
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str())
                        .collect::<Vec<_>>()
                        .join(",")
                })
                .unwrap_or_else(|| "*".to_string());
            let new_allow = Text::new("Allowed from (comma separated IDs or *):")
                .with_default(&current_allow)
                .prompt()?;
            obj.insert(
                "allowFrom".to_string(),
                Value::Array(
                    new_allow
                        .split(',')
                        .map(|s| Value::String(s.trim().to_string()))
                        .collect(),
                ),
            );
            obj.remove("allow_from");

            obj.insert(
                "appId".to_string(),
                Value::String(
                    Text::new("App ID:")
                        .with_default(
                            obj.get("appId")
                                .or_else(|| obj.get("app_id"))
                                .and_then(Value::as_str)
                                .unwrap_or(""),
                        )
                        .with_validator(|s: &str| {
                            if s.trim().is_empty() {
                                Ok(Validation::Invalid("Cannot be empty".into()))
                            } else {
                                Ok(Validation::Valid)
                            }
                        })
                        .prompt()?,
                ),
            );
            obj.remove("app_id");

            obj.insert(
                "appSecret".to_string(),
                Value::String(
                    Text::new("App Secret:")
                        .with_default(
                            obj.get("appSecret")
                                .or_else(|| obj.get("app_secret"))
                                .and_then(Value::as_str)
                                .unwrap_or(""),
                        )
                        .with_validator(|s: &str| {
                            if s.trim().is_empty() {
                                Ok(Validation::Invalid("Cannot be empty".into()))
                            } else {
                                Ok(Validation::Valid)
                            }
                        })
                        .prompt()?,
                ),
            );
            obj.remove("app_secret");

            obj.insert(
                "encryptKey".to_string(),
                Value::String(
                    Text::new("Encrypt Key:")
                        .with_default(
                            obj.get("encryptKey")
                                .or_else(|| obj.get("encrypt_key"))
                                .and_then(Value::as_str)
                                .unwrap_or(""),
                        )
                        .with_validator(|s: &str| {
                            if s.trim().is_empty() {
                                Ok(Validation::Invalid("Cannot be empty".into()))
                            } else {
                                Ok(Validation::Valid)
                            }
                        })
                        .prompt()?,
                ),
            );
            obj.remove("encrypt_key");

            obj.insert(
                "verificationToken".to_string(),
                Value::String(
                    Text::new("Verification Token:")
                        .with_default(
                            obj.get("verificationToken")
                                .or_else(|| obj.get("verification_token"))
                                .and_then(Value::as_str)
                                .unwrap_or(""),
                        )
                        .with_validator(|s: &str| {
                            if s.trim().is_empty() {
                                Ok(Validation::Invalid("Cannot be empty".into()))
                            } else {
                                Ok(Validation::Valid)
                            }
                        })
                        .prompt()?,
                ),
            );
            obj.remove("verification_token");

            obj.insert(
                "webhookPath".to_string(),
                Value::String(
                    Text::new("Webhook Path:")
                        .with_default(
                            obj.get("webhookPath")
                                .or_else(|| obj.get("webhook_path"))
                                .and_then(Value::as_str)
                                .unwrap_or("/feishu/events"),
                        )
                        .prompt()?,
                ),
            );
            obj.remove("webhook_path");

            obj.insert(
                "groupPolicy".to_string(),
                Value::String(
                    Select::new(
                        "Group Policy:",
                        vec!["mention".to_string(), "always".to_string()],
                    )
                    .with_starting_cursor(
                        if obj.get("groupPolicy").and_then(Value::as_str) == Some("always") {
                            1
                        } else {
                            0
                        },
                    )
                    .prompt()?,
                ),
            );
            obj.remove("group_policy");

            obj.insert(
                "replyToMessage".to_string(),
                Value::Bool(
                    Confirm::new("Reply to message?")
                        .with_default(
                            obj.get("replyToMessage")
                                .or_else(|| obj.get("reply_to_message"))
                                .and_then(Value::as_bool)
                                .unwrap_or(true),
                        )
                        .prompt()?,
                ),
            );
            obj.remove("reply_to_message");

            obj.insert(
                "reactEmoji".to_string(),
                Value::String(
                    Text::new("React Emoji:")
                        .with_default(
                            obj.get("reactEmoji")
                                .or_else(|| obj.get("react_emoji"))
                                .and_then(Value::as_str)
                                .unwrap_or("THUMBSUP"),
                        )
                        .prompt()?,
                ),
            );
            obj.remove("react_emoji");
        } else {
            // Basic fields for other channels (Email, Telegram, etc.)
            let enabled = Confirm::new("Enabled?")
                .with_default(obj.get("enabled").and_then(Value::as_bool).unwrap_or(false))
                .prompt()?;
            obj.insert("enabled".to_string(), Value::Bool(enabled));

            let current_allow = obj
                .get("allowFrom")
                .or_else(|| obj.get("allow_from"))
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str())
                        .collect::<Vec<_>>()
                        .join(",")
                })
                .unwrap_or_else(|| "*".to_string());

            let new_allow = Text::new("Allowed from (comma separated IDs or *):")
                .with_default(&current_allow)
                .prompt()?;

            let allow_arr = new_allow
                .split(',')
                .map(|s| Value::String(s.trim().to_string()))
                .collect::<Vec<_>>();
            obj.insert("allowFrom".to_string(), Value::Array(allow_arr));

            if selected_channel_name == "telegram" {
                let token = Text::new("Bot Token:")
                    .with_default(obj.get("token").and_then(Value::as_str).unwrap_or(""))
                    .prompt()?;
                obj.insert("token".to_string(), Value::String(token));
            }
        }
    }

    config
        .channels
        .sections
        .insert(selected_channel_name.clone(), channel_config);

    let path = config.save(config_path)?;
    println!(
        "\n{}",
        Style::new()
            .green()
            .bold()
            .apply_to("Configuration saved successfully!")
    );
    println!("Config file: {}", path.display());

    // Print final config
    println!(
        "\n{}",
        Style::new().magenta().apply_to("Final Channel Config:")
    );
    println!(
        "{}",
        serde_json::to_string_pretty(&config.channels.sections.get(&selected_channel_name))?
    );

    Ok(())
}
