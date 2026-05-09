use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use console::Style;

use xbot::channels::discover_all;
use xbot::config::Config;
use xbot::storage::MessageBus;

pub async fn run_channels_list() -> Result<()> {
    println!(
        "{}",
        Style::new().cyan().bold().apply_to("─ Available Channels")
    );
    println!();

    let channels = discover_all();
    for (name, descriptor) in &channels {
        println!(
            "  {} {}",
            Style::new().bold().apply_to(&descriptor.display_name),
            Style::new().dim().apply_to(format!("({name})"))
        );
    }
    println!("\n{} channels available.", channels.len());
    Ok(())
}

pub async fn run_channels_status(config_path: Option<&Path>) -> Result<()> {
    let config = Config::load(config_path)?;

    println!(
        "{}",
        Style::new().cyan().bold().apply_to("─ Channel Status")
    );
    println!();

    let channels = discover_all();
    for (name, descriptor) in &channels {
        let section = config.channels.section(name);
        let enabled = section
            .and_then(|s| s.get("enabled"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let status = if enabled {
            Style::new().green().apply_to("enabled")
        } else {
            Style::new().dim().apply_to("disabled")
        };

        let allow_from = section
            .and_then(|s| s.get("allowFrom").or_else(|| s.get("allow_from")))
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_else(|| "none".to_string());

        println!(
            "  {:<12} [{status}]  allowFrom: {allow_from}",
            descriptor.display_name
        );
    }
    Ok(())
}

pub async fn run_channels_login(config_path: Option<&Path>, name: Option<String>) -> Result<()> {
    let config = Config::load(config_path)?;

    let descriptors = discover_all();
    let channel_name = match name {
        Some(n) => n,
        None => {
            let names: Vec<String> = descriptors.keys().cloned().collect();
            inquire::Select::new("Select channel to login:", names).prompt()?
        }
    };

    let descriptor = descriptors
        .get(&channel_name)
        .ok_or_else(|| anyhow!("unknown channel: {channel_name}"))?;

    println!(
        "{}",
        Style::new()
            .cyan()
            .bold()
            .apply_to(format!("─ Login: {}", descriptor.display_name))
    );
    println!();

    let section = config
        .channels
        .section(&channel_name)
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));

    let bus = MessageBus::new(128);
    let ws = &config.agents.defaults.workspace;
    let workspace = if ws.is_empty() {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    } else {
        PathBuf::from(ws)
    };

    let channel = (descriptor.factory)(
        section,
        bus,
        workspace,
        config.channels.transcription_api_key.clone(),
    )?;

    if channel.supports_login() {
        println!(
            "{}",
            Style::new()
                .yellow()
                .apply_to("Starting interactive login...")
        );
        println!();
        match channel.login(true).await {
            Ok(true) => {
                println!();
                println!(
                    "{}",
                    Style::new().green().bold().apply_to("Login successful.")
                );
            }
            Ok(false) => {
                println!();
                println!(
                    "{}",
                    Style::new().red().apply_to("Login was not completed.")
                );
            }
            Err(e) => {
                println!();
                println!(
                    "{} {}",
                    Style::new().red().bold().apply_to("Login error:"),
                    e
                );
            }
        }
    } else {
        println!(
            "Channel '{}' does not support interactive login.",
            descriptor.display_name
        );
        println!("It uses API tokens/keys configured in config.json.\n");
        println!("{}", Style::new().cyan().apply_to("─ Setup Instructions"));
        println!();
        println!("{}", channel.setup_instructions());
    }

    Ok(())
}

pub async fn run_channels_setup(config_path: Option<&Path>, name: Option<String>) -> Result<()> {
    let _config = Config::load(config_path)?;

    let descriptors = discover_all();
    let channel_name = match name {
        Some(n) => n,
        None => {
            let names: Vec<String> = descriptors.keys().cloned().collect();
            inquire::Select::new("Select channel for setup instructions:", names).prompt()?
        }
    };

    let descriptor = descriptors
        .get(&channel_name)
        .ok_or_else(|| anyhow!("unknown channel: {channel_name}"))?;

    println!(
        "{}",
        Style::new()
            .cyan()
            .bold()
            .apply_to(format!("─ Setup: {}", descriptor.display_name))
    );
    println!();

    let section = serde_json::json!({});
    let bus = MessageBus::new(1);
    let workspace = PathBuf::from(".");
    let channel = (descriptor.factory)(section, bus, workspace, String::new())?;

    println!("{}", channel.setup_instructions());
    Ok(())
}
