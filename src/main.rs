mod cli;
mod tui;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
};

use anyhow::{Result, anyhow};
use clap::{Parser, Subcommand};
use cli::{
    CliShell, TurnSummary, run_channels_list, run_channels_login, run_channels_setup,
    run_channels_status, run_config_channel, run_config_provider, run_skills_init, run_skills_list,
};
use colored::*;
use local_ip_address::local_ip;

use xbot::channels::ChannelManager;
use xbot::config::{Config, expand_tilde};
use xbot::cron::CronService;
use xbot::engine::AgentLoop;
use xbot::observability::{InstrumentedProvider, RuntimeTelemetry};
use xbot::providers::registry::normalize_provider_name;
use xbot::providers::{ProviderModelInfo, SharedProvider};
use xbot::runtime::{
    AgentRuntime, HeartbeatService, build_gateway_router, build_provider_client,
    validate_run_config,
};
use xbot::storage::{InboundMessage, MessageBus, OutboundMessage, SessionManager};
use xbot::tools::MessageSendCallback;
use xbot::util::{
    sync_workspace_templates, sync_workspace_templates_without_memory, truncate_chars_ellipsis,
    workspace_state_dir,
};

#[derive(Parser)]
#[command(name = "xbot", about = "Rust-native autonomous bot runtime")]
struct Cli {
    #[arg(long)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Onboard {
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
    Chat {
        #[arg(long)]
        model: Option<String>,
        #[arg(long, conflicts_with = "workspace")]
        global: bool,
        #[arg(long)]
        workspace: Option<PathBuf>,
        prompt: Vec<String>,
    },
    Repl {
        #[arg(long)]
        model: Option<String>,
        #[arg(long, conflicts_with = "workspace")]
        global: bool,
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
    Run {
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
    Status {
        #[arg(long)]
        model: Option<String>,
    },
    Sessions,
    Jobs,
    PrintConfig,
    Config {
        #[arg(long)]
        provider: bool,
        #[arg(long)]
        channel: bool,
    },
    Channels {
        #[command(subcommand)]
        subcommand: ChannelsCommand,
    },
    Skills {
        #[command(subcommand)]
        subcommand: SkillsCommand,
    },
}

#[derive(Subcommand)]
enum ChannelsCommand {
    /// List all available channels
    List,
    /// Show enabled/disabled status of each channel
    Status,
    /// Interactive login for channels that support it (e.g. weixin QR code)
    Login { name: Option<String> },
    /// Show setup instructions for a channel (how to obtain tokens/keys)
    Setup { name: Option<String> },
}

#[derive(Subcommand)]
enum SkillsCommand {
    List,
    Init { name: String },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Onboard { workspace } => onboard(cli.config.as_deref(), workspace.as_deref()),
        Command::Chat {
            model,
            global,
            workspace,
            prompt,
        } => {
            chat(
                cli.config.as_deref(),
                model,
                workspace.as_deref(),
                global,
                prompt,
            )
            .await
        }
        Command::Repl {
            model,
            global,
            workspace,
        } => repl(cli.config.as_deref(), model, workspace.as_deref(), global).await,
        Command::Run { model, workspace } => {
            run(cli.config.as_deref(), model, workspace.as_deref()).await
        }
        Command::Status { model } => status(cli.config.as_deref(), model).await,
        Command::Sessions => sessions(cli.config.as_deref()),
        Command::Jobs => jobs(cli.config.as_deref()),
        Command::PrintConfig => {
            let config = Config::load(cli.config.as_deref())?;
            println!("{}", serde_json::to_string_pretty(&config)?);
            Ok(())
        }
        Command::Config { provider, channel } => {
            config_cmd(cli.config.as_deref(), provider, channel).await
        }
        Command::Channels { subcommand } => match subcommand {
            ChannelsCommand::List => run_channels_list().await,
            ChannelsCommand::Status => run_channels_status(cli.config.as_deref()).await,
            ChannelsCommand::Login { name } => {
                run_channels_login(cli.config.as_deref(), name).await
            }
            ChannelsCommand::Setup { name } => {
                run_channels_setup(cli.config.as_deref(), name).await
            }
        },
        Command::Skills { subcommand } => match subcommand {
            SkillsCommand::List => run_skills_list(cli.config.as_deref()).await,
            SkillsCommand::Init { name } => run_skills_init(&name, cli.config.as_deref()).await,
        },
    }
}

async fn config_cmd(config_path: Option<&Path>, provider: bool, channel: bool) -> Result<()> {
    if provider {
        run_config_provider(config_path).await?;
    } else if channel {
        run_config_channel(config_path).await?;
    } else {
        println!("Please specify either --provider or --channel");
    }
    Ok(())
}

fn onboard(config_path: Option<&Path>, workspace_override: Option<&Path>) -> Result<()> {
    let mut config = Config::load(config_path)?;
    if let Some(workspace) = workspace_override {
        config.agents.defaults.workspace = workspace.display().to_string();
    }
    let path = config.save(config_path)?;
    let workspace = config.workspace_path();
    let created = sync_workspace_templates(&workspace)?;
    println!("Config: {}", path.display());
    println!("Workspace: {}", workspace.display());
    if created.is_empty() {
        println!("Workspace templates already present.");
    } else {
        for path in created {
            println!("Created {}", path.display());
        }
    }
    print_onboard_next_steps(config_path);
    Ok(())
}

fn print_onboard_next_steps(config_path: Option<&Path>) {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("xbot"));
    let prefix = onboard_command_prefix(&exe);
    println!();
    println!("{}", "Next steps: configure provider and channels".bold());
    println!(
        "  {}",
        onboard_config_command(&prefix, config_path, "--provider")
            .cyan()
            .bold()
    );
    println!(
        "  {}",
        onboard_config_command(&prefix, config_path, "--channel")
            .cyan()
            .bold()
    );
}

fn onboard_command_prefix(exe: &Path) -> String {
    let parts = exe
        .components()
        .map(|part| part.as_os_str().to_string_lossy())
        .collect::<Vec<_>>();
    if parts
        .windows(2)
        .any(|window| window == ["target", "release"])
    {
        "cargo run --release --".to_string()
    } else if parts.windows(2).any(|window| window == ["target", "debug"]) {
        "cargo run --".to_string()
    } else {
        "xbot".to_string()
    }
}

fn onboard_config_command(prefix: &str, config_path: Option<&Path>, flag: &str) -> String {
    match config_path {
        Some(path) => format!("{prefix} --config {} config {flag}", shell_arg(path)),
        None => format!("{prefix} config {flag}"),
    }
}

fn shell_arg(path: &Path) -> String {
    let text = path.display().to_string();
    if text
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '/' | '.' | '_' | '-' | '~'))
    {
        return text;
    }
    format!("'{}'", text.replace('\'', "'\\''"))
}

async fn chat(
    config_path: Option<&Path>,
    model: Option<String>,
    workspace_override: Option<&Path>,
    use_global_workspace: bool,
    prompt: Vec<String>,
) -> Result<()> {
    let prompt = if prompt.is_empty() {
        return Err(anyhow!("chat requires a prompt"));
    } else {
        prompt.join(" ")
    };
    let built = build_agent(config_path, model, workspace_override, use_global_workspace).await?;
    if let Some(notice) = &built.startup_notice {
        eprintln!("{notice}");
    }

    let session_key = chat_select_session(&built.workspace, &built.session_key, &built.chat_id)?;

    let shell = CliShell::new(
        &built.workspace,
        &built.cwd,
        &built.model,
        &built.provider_name,
    )?;
    let stream = shell.stream_renderer();
    built
        .agent
        .set_progress_sender(Some(cli_progress_callback(stream.clone())));
    built
        .agent
        .set_approval_callback(Some(cli_approval_callback(stream.clone())));
    let subagent_stream = stream.clone();
    built
        .agent
        .set_subagent_notification_callback(Some(Arc::new(move |notif| {
            use xbot::engine::subtasks::SubagentNotification;
            match notif {
                SubagentNotification::Started { label, .. } => {
                    subagent_stream.subagent_event(&label, "started");
                }
                SubagentNotification::Completed { label, .. } => {
                    subagent_stream.subagent_event(&label, "completed");
                }
                SubagentNotification::Failed { label, .. } => {
                    subagent_stream.subagent_event(&label, "failed");
                }
                _ => {}
            }
        })));
    let started = Instant::now();
    stream.start_waiting();
    let result = built
        .agent
        .process_direct_stream(
            &prompt,
            &session_key,
            "cli",
            &built.chat_id,
            Some(stream.callback()),
            Some(stream.reasoning_callback()),
        )
        .await;
    stream.stop_spinner();
    match result {
        Ok(Some(response)) => {
            let summary = turn_summary(&built.agent, started.elapsed())?;
            stream.finish(
                &response.content,
                response.reasoning_content.as_deref(),
                &summary,
            );
        }
        Ok(None) => {
            let summary = turn_summary(&built.agent, started.elapsed())?;
            stream.finish_empty("no direct reply", &summary);
        }
        Err(e) => {
            stream.finish_error(&format!("{e:#}"));
            return Err(e);
        }
    }
    Ok(())
}

fn chat_select_session(workspace: &Path, default_key: &str, chat_id: &str) -> Result<String> {
    use std::io::{IsTerminal, Write};

    let mut manager = SessionManager::new(workspace)?;
    let summaries = manager.list_session_summaries()?;
    let mut cli_sessions: Vec<_> = summaries
        .into_iter()
        .filter(|s| s.key.starts_with("cli:") && s.message_count > 0)
        .collect();

    if cli_sessions.is_empty() {
        return Ok(default_key.to_string());
    }

    if !std::io::stdin().is_terminal() {
        if cli_sessions.len() == 1 && cli_sessions[0].key == default_key {
            let s = &cli_sessions[0];
            eprintln!(
                "{}",
                format!(
                    "Continuing session: {} ({} msgs, {}, updated {})",
                    s.title,
                    s.message_count,
                    format_session_context_tokens(s.context_tokens),
                    format_relative_time(&s.updated_at),
                )
                .dimmed()
            );
        }
        return Ok(default_key.to_string());
    }

    loop {
        eprintln!("\n{}", "Available sessions:".bold());
        eprintln!("  {} {} (new session)", "0.".bold(), chat_id.cyan().bold());
        for (i, s) in cli_sessions.iter().enumerate() {
            let current = if s.key == default_key { " ←" } else { "" };
            eprintln!(
                "  {} {} - {} msgs, {}, updated {}{}",
                format!("{}.", i + 1).bold(),
                truncate_display(&s.title, 50).cyan(),
                s.message_count,
                format_session_context_tokens(s.context_tokens),
                format_relative_time(&s.updated_at),
                current.yellow().bold(),
            );
        }
        eprintln!(
            "\n  {}",
            "Enter number to select, d<number> to delete (e.g. d2)".dimmed()
        );
        eprint!("  Select session [default: continue current]: ");
        let _ = std::io::stderr().flush();

        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        let choice = input.trim();

        if choice.is_empty() {
            return Ok(default_key.to_string());
        }
        if choice == "0" {
            let new_key = format!("cli:{}:{:016x}", chat_id, rand_u64());
            return Ok(new_key);
        }

        if let Some(rest) = choice
            .strip_prefix('d')
            .or_else(|| choice.strip_prefix('D'))
        {
            if let Ok(idx) = rest.trim().parse::<usize>() {
                if idx >= 1 && idx <= cli_sessions.len() {
                    let target = &cli_sessions[idx - 1];
                    eprint!(
                        "  Delete \"{}\"? (y/N): ",
                        truncate_display(&target.title, 40)
                    );
                    let _ = std::io::stderr().flush();
                    let mut confirm = String::new();
                    std::io::stdin().read_line(&mut confirm)?;
                    if confirm.trim().eq_ignore_ascii_case("y") {
                        let del_key = target.key.clone();
                        let was_default = del_key == default_key;
                        let _ = manager.delete(&del_key);
                        cli_sessions.remove(idx - 1);
                        eprintln!("  {}", "Session deleted.".dimmed());
                        if cli_sessions.is_empty() {
                            let new_key = format!("cli:{}:{:016x}", chat_id, rand_u64());
                            return Ok(new_key);
                        }
                        if was_default {
                            continue;
                        }
                    }
                    continue;
                }
            }
        }

        if let Ok(idx) = choice.parse::<usize>() {
            if idx >= 1 && idx <= cli_sessions.len() {
                return Ok(cli_sessions[idx - 1].key.clone());
            }
        }
        return Ok(default_key.to_string());
    }
}

fn rand_u64() -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    std::time::SystemTime::now().hash(&mut hasher);
    std::process::id().hash(&mut hasher);
    hasher.finish()
}

fn format_relative_time(iso: &str) -> String {
    use chrono::{DateTime, Utc};
    let Ok(dt) = iso.parse::<DateTime<Utc>>() else {
        return iso.to_string();
    };
    let now = Utc::now();
    let dur = now.signed_duration_since(dt);
    if dur.num_seconds() < 60 {
        "just now".to_string()
    } else if dur.num_minutes() < 60 {
        format!("{}m ago", dur.num_minutes())
    } else if dur.num_hours() < 24 {
        format!("{}h ago", dur.num_hours())
    } else {
        format!("{}d ago", dur.num_days())
    }
}

fn truncate_display(s: &str, max: usize) -> String {
    truncate_chars_ellipsis(s, max)
}

fn format_session_context_tokens(tokens: Option<usize>) -> String {
    match tokens {
        Some(tokens) if tokens >= 1000 => format!("{}k context tokens", tokens / 1000),
        Some(tokens) => format!("{tokens} context tokens"),
        None => "context tokens unknown".to_string(),
    }
}

async fn repl(
    config_path: Option<&Path>,
    model: Option<String>,
    workspace_override: Option<&Path>,
    use_global_workspace: bool,
) -> Result<()> {
    let built = build_agent(config_path, model, workspace_override, use_global_workspace).await?;
    let BuiltAgent {
        agent,
        model,
        provider_name,
        workspace,
        cwd,
        session_key,
        chat_id,
        startup_notice,
        subagent_model,
    } = built;
    let session_message_count = repl_session_message_count(&workspace, &session_key)?;
    let context_status = repl_session_context_status(&agent, &session_key).await?;
    let display_model =
        repl_session_model(&workspace, &session_key)?.unwrap_or_else(|| model.clone());

    if let Some(notice) = &startup_notice {
        eprintln!("{notice}");
    }

    let agent = Arc::new(agent);
    configure_model_switch_persistence(&agent, config_path);

    tui::run_tui_repl(
        agent,
        display_model,
        provider_name,
        workspace,
        cwd,
        session_key,
        chat_id,
        session_message_count,
        context_status,
        subagent_model,
    )
    .await
}

async fn run(
    config_path: Option<&Path>,
    model_override: Option<String>,
    workspace_override: Option<&Path>,
) -> Result<()> {
    let config = Config::load(config_path)?;
    let resolved_config_path = config_path
        .map(Path::to_path_buf)
        .unwrap_or_else(Config::default_path);
    let workspace = resolve_run_workspace(&config, workspace_override);
    sync_workspace_templates(&workspace)?;
    let model = model_override.unwrap_or_else(|| config.agents.defaults.model.clone());
    validate_run_config(&config, &model)?;
    let (provider_name, provider_cfg) = config
        .provider_for_model(Some(&model))
        .ok_or_else(|| anyhow!("no configured provider matched model '{model}'"))?;
    let provider = build_provider_client(
        &provider_name,
        &provider_cfg,
        &model,
        config.provider_api_base_for_model(Some(&model)),
        config.tools.web.proxy.as_deref(),
        config.agents.defaults.temperature,
    )?;
    let startup_model = resolve_startup_model(&provider, &model).await;
    if let Some(notice) = &startup_model.notice {
        eprintln!("{notice}");
    }
    let telemetry = RuntimeTelemetry::new(
        provider_name.clone(),
        startup_model.active_model.clone(),
        config.provider_api_base_for_model(Some(&model)),
    );
    let provider: SharedProvider = Arc::new(InstrumentedProvider::new(provider, telemetry.clone()));
    let bus = MessageBus::with_telemetry(256, Some(telemetry.clone()));
    let agent_slot: Arc<std::sync::Mutex<Option<Arc<AgentLoop>>>> =
        Arc::new(std::sync::Mutex::new(None));
    let cron_bus = bus.clone();
    let cron_agent_slot = agent_slot.clone();
    let cron_service = CronService::with_callback(
        workspace_state_dir(&workspace)
            .join("cron")
            .join("jobs.json"),
        move |job| {
            let cron_bus = cron_bus.clone();
            let cron_agent_slot = cron_agent_slot.clone();
            async move {
                if job.payload.kind != "agent_turn" {
                    return Ok(());
                }
                let Some(agent) = cron_agent_slot
                    .lock()
                    .expect("cron agent slot lock poisoned")
                    .clone()
                else {
                    return Ok(());
                };
                let channel = job
                    .payload
                    .channel
                    .clone()
                    .unwrap_or_else(|| "system".to_string());
                let chat_id = job.payload.to.clone().unwrap_or_else(|| "cron".to_string());
                let session_key = format!("cron:{channel}:{chat_id}");
                if let Some(outbound) = agent
                    .process_direct(&job.payload.message, &session_key, &channel, &chat_id)
                    .await?
                {
                    if job.payload.deliver {
                        cron_bus.publish_outbound(outbound).await?;
                    }
                }
                Ok(())
            }
        },
    );

    let agent = Arc::new(
        build_agent_for_workspace(
            &config,
            &workspace,
            provider,
            Some(startup_model.active_model.clone()),
            Some(cron_service.clone()),
            true,
        )
        .await?,
    );
    configure_model_switch_persistence(&agent, config_path);
    *agent_slot.lock().expect("agent slot lock poisoned") = Some(agent.clone());

    let runtime = AgentRuntime::new(
        agent.clone(),
        bus.clone(),
        config.agents.defaults.max_concurrent_requests,
    );
    let heartbeat_service = build_heartbeat_service(
        &config,
        workspace.as_path(),
        bus.clone(),
        agent_slot.clone(),
        startup_model.active_model.clone(),
        provider_name.clone(),
    );
    runtime.start().await?;
    cron_service.start().await?;

    let manager = Arc::new(ChannelManager::new(
        config.channels.clone(),
        bus.clone(),
        workspace.clone(),
    )?);
    manager.start_all().await?;
    if let Some(heartbeat_service) = &heartbeat_service {
        heartbeat_service.start().await?;
    }

    let router = build_gateway_router(
        &manager,
        &config,
        Some(agent.clone()),
        Some(cron_service.clone()),
        heartbeat_service.clone(),
        Some(telemetry.clone()),
    )?
    .expect("gateway router is always available");
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
    let host = config.gateway.host.clone();
    let port = config.gateway.port;
    let listener = tokio::net::TcpListener::bind(format!("{host}:{port}")).await?;
    let gateway_bind = format!("http://{host}:{port}");
    let gateway_public = gateway_url(&host, port, "/");
    let admin_url = gateway_url(&host, port, &config.gateway.admin.path);
    let metrics_url = gateway_url(&host, port, &config.gateway.metrics.path);
    let server_task = tokio::spawn(async move {
        let _ = axum::serve(listener, router)
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.changed().await;
            })
            .await;
    });

    let enabled = manager.enabled_channels();
    let snapshot = agent.snapshot()?;
    let context_status = format_max_context_length(snapshot.context_window_tokens);
    println!(
        "{}",
        format_run_welcome(
            resolved_config_path.as_path(),
            workspace.as_path(),
            &config,
            &startup_model.active_model,
            &provider_name,
            &context_status,
            &enabled,
            &gateway_bind,
            &gateway_public,
            &admin_url,
            &metrics_url,
        )
    );

    tokio::signal::ctrl_c().await?;
    let _ = shutdown_tx.send(true);
    let _ = server_task.await;
    manager.stop_all().await?;
    if let Some(heartbeat_service) = &heartbeat_service {
        heartbeat_service.stop().await;
    }
    cron_service.stop();
    runtime.stop().await;
    Ok(())
}

async fn status(config_path: Option<&Path>, model_override: Option<String>) -> Result<()> {
    let config = Config::load(config_path)?;
    let workspace = config.workspace_path();
    let model = model_override.unwrap_or_else(|| config.agents.defaults.model.clone());
    let provider_name = config
        .provider_name_for_model(Some(&model))
        .unwrap_or_else(|| "unknown".to_string());
    let api_base = config.provider_api_base_for_model(Some(&model));
    let api_key = config
        .provider_for_model(Some(&model))
        .map(|(_, cfg)| cfg.api_key)
        .filter(|k| !k.trim().is_empty());
    let system = xbot::observability::collect_system_snapshot().await;
    let provider = xbot::observability::collect_provider_model_snapshot(
        &provider_name,
        &model,
        api_base.as_deref(),
        api_key.as_deref(),
    )
    .await;
    let session_manager = xbot::storage::SessionManager::new(&workspace)?;
    let sessions = session_manager.list_session_summaries()?;
    let cron = CronService::new(
        workspace_state_dir(&workspace)
            .join("cron")
            .join("jobs.json"),
    );
    let (cron_running, cron_jobs, next_run) = cron.status()?;

    println!("Workspace: {}", workspace.display());
    println!("Model: {model}");
    println!("Provider: {provider_name}");
    println!(
        "API Base: {}",
        api_base.unwrap_or_else(|| "(default)".to_string())
    );
    println!(
        "Admin UI: {}",
        format_gateway_url(&gateway_url(
            &config.gateway.host,
            config.gateway.port,
            &config.gateway.admin.path
        ))
    );
    println!(
        "Metrics: {}",
        format_gateway_url(&gateway_url(
            &config.gateway.host,
            config.gateway.port,
            &config.gateway.metrics.path
        ))
    );
    println!("Sessions: {}", sessions.len());
    println!("Cron Jobs: {cron_jobs}");
    println!("Cron Running: {cron_running}");
    println!(
        "Next Cron Run (ms): {}",
        next_run
            .map(|value| value.to_string())
            .unwrap_or_else(|| "n/a".to_string())
    );
    println!("CPU Usage: {:.2}%", system.cpu_usage_pct);
    println!(
        "Memory: {} / {}",
        system.used_memory_bytes, system.total_memory_bytes
    );
    println!("Resolved Model ID: {}", provider.model_id);
    println!(
        "Resolved Model Size: {}",
        provider
            .model_size_bytes
            .map(|value| value.to_string())
            .unwrap_or_else(|| "n/a".to_string())
    );
    if !provider.available_models.is_empty() {
        println!("Available Models: {}", provider.available_models.join(", "));
    }
    Ok(())
}

fn sessions(config_path: Option<&Path>) -> Result<()> {
    let config = Config::load(config_path)?;
    let manager = xbot::storage::SessionManager::new(&config.workspace_path())?;
    for session in manager.list_session_summaries()? {
        println!(
            "{} | updated={} | messages={} | consolidated={}",
            session.key, session.updated_at, session.message_count, session.last_consolidated
        );
    }
    Ok(())
}

fn jobs(config_path: Option<&Path>) -> Result<()> {
    let config = Config::load(config_path)?;
    let cron = CronService::new(
        workspace_state_dir(&config.workspace_path())
            .join("cron")
            .join("jobs.json"),
    );
    for job in cron.list_jobs(true)? {
        println!(
            "{} | enabled={} | next_run={:?} | last_status={:?}",
            job.name, job.enabled, job.state.next_run_at_ms, job.state.last_status
        );
    }
    Ok(())
}

struct BuiltAgent {
    agent: AgentLoop,
    model: String,
    provider_name: String,
    workspace: PathBuf,
    cwd: PathBuf,
    session_key: String,
    chat_id: String,
    startup_notice: Option<String>,
    subagent_model: Option<String>,
}

async fn build_agent(
    config_path: Option<&Path>,
    model_override: Option<String>,
    workspace_override: Option<&Path>,
    use_global_workspace: bool,
) -> Result<BuiltAgent> {
    let config = Config::load(config_path)?;
    let cwd = std::env::current_dir()?
        .canonicalize()
        .unwrap_or_else(|_| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let model = model_override.unwrap_or_else(|| config.agents.defaults.model.clone());
    let (provider_name, provider_cfg) = config
        .provider_for_model(Some(&model))
        .ok_or_else(|| anyhow!("no configured provider matched model '{model}'"))?;
    let provider = build_provider_client(
        &provider_name,
        &provider_cfg,
        &model,
        config.provider_api_base_for_model(Some(&model)),
        config.tools.web.proxy.as_deref(),
        config.agents.defaults.temperature,
    )?;
    let startup_model = resolve_startup_model(&provider, &model).await;
    let subagent_model = config.agents.subagents.model.trim();
    let subagent_model = (!subagent_model.is_empty()).then(|| subagent_model.to_string());
    let workspace = resolve_cli_workspace(&config, &cwd, workspace_override, use_global_workspace);
    sync_workspace_templates_without_memory(&workspace)?;
    let agent = build_agent_for_workspace(
        &config,
        &workspace,
        provider,
        Some(startup_model.active_model.clone()),
        None,
        false,
    )
    .await?;
    configure_model_switch_persistence(&agent, config_path);
    let session_key = cli_session_key(&cwd);
    let chat_id = cwd
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "direct".to_string());
    Ok(BuiltAgent {
        agent,
        model: startup_model.active_model,
        provider_name,
        workspace,
        cwd,
        session_key,
        chat_id,
        startup_notice: startup_model.notice,
        subagent_model,
    })
}

fn configure_model_switch_persistence(agent: &AgentLoop, config_path: Option<&Path>) {
    let config_path = config_path.map(Path::to_path_buf);
    agent.set_model_switch_callback(Some(Arc::new(move |model, context_window_tokens| {
        let mut config = Config::load(config_path.as_deref())?;
        config.agents.defaults.model = model;
        if let Some(context_window_tokens) = context_window_tokens.filter(|value| *value > 0) {
            config.agents.defaults.context_window_tokens = context_window_tokens;
        }
        config.save(config_path.as_deref())?;
        Ok(())
    })));
}

struct StartupModelResolution {
    active_model: String,
    notice: Option<String>,
}

async fn resolve_startup_model(
    provider: &SharedProvider,
    requested_model: &str,
) -> StartupModelResolution {
    let models = match provider.list_models().await {
        Ok(models) if !models.is_empty() => models,
        _ => {
            return StartupModelResolution {
                active_model: requested_model.to_string(),
                notice: None,
            };
        }
    };
    if let Some(selected_model) = resolve_startup_model_selection(&models, requested_model) {
        return StartupModelResolution {
            active_model: selected_model.id.clone(),
            notice: None,
        };
    }
    let selected_model = &models[0];
    StartupModelResolution {
        active_model: selected_model.id.clone(),
        notice: Some(format!(
            "Warning: requested model '{requested_model}' was not found in provider /models; using '{}' instead. Run `/model {}` to persist it.",
            selected_model.id, selected_model.id
        )),
    }
}

fn resolve_startup_model_selection<'a>(
    models: &'a [ProviderModelInfo],
    requested_model: &str,
) -> Option<&'a ProviderModelInfo> {
    models
        .iter()
        .find(|item| item.id == requested_model)
        .or_else(|| {
            let requested = requested_model.to_ascii_lowercase();
            let mut matches = models
                .iter()
                .filter(|item| item.id.to_ascii_lowercase() == requested);
            let first = matches.next()?;
            matches.next().is_none().then_some(first)
        })
        .or_else(|| {
            let basename = requested_model
                .trim_end_matches(['/', '\\'])
                .rsplit(['/', '\\'])
                .next()?;
            let mut matches = models.iter().filter(|item| {
                item.id
                    .trim_end_matches(['/', '\\'])
                    .rsplit(['/', '\\'])
                    .next()
                    .map(|name| name.eq_ignore_ascii_case(basename))
                    .unwrap_or(false)
            });
            let first = matches.next()?;
            matches.next().is_none().then_some(first)
        })
}

async fn build_agent_for_workspace(
    config: &Config,
    workspace: &Path,
    provider: SharedProvider,
    model: Option<String>,
    cron_service: Option<CronService>,
    memory_enabled: bool,
) -> Result<AgentLoop> {
    let main_model = model
        .clone()
        .unwrap_or_else(|| provider.default_model().to_string());
    let subagent = build_subagent_provider_from_config(config, provider.clone(), &main_model)?;

    AgentLoop::new_with_subagent_provider(
        provider,
        workspace,
        model,
        subagent.provider,
        subagent.model,
        config.agents.defaults.max_tool_iterations,
        config.agents.defaults.max_concurrent_tools,
        config.agents.defaults.context_window_tokens,
        config.agents.defaults.memory_max_bytes,
        config.tools.web.search.clone(),
        config.tools.web.proxy.clone(),
        config.tools.exec.clone(),
        config.tools.restrict_to_workspace,
        cron_service,
        memory_enabled,
        &config.tools.mcp_servers,
    )
    .await
}

struct BuiltSubagentProvider {
    provider: Option<SharedProvider>,
    model: Option<String>,
}

fn build_subagent_provider_from_config(
    config: &Config,
    main_provider: SharedProvider,
    main_model: &str,
) -> Result<BuiltSubagentProvider> {
    let subagent_model = config.subagent_model(main_model);
    let inherits_main = config.agents.subagents.model.trim().is_empty()
        && normalize_provider_name(config.agents.subagents.provider.trim()) == "auto"
        && config.agents.subagents.api_base.is_none();
    if inherits_main {
        return Ok(BuiltSubagentProvider {
            provider: None,
            model: None,
        });
    }

    let Some((provider_name, provider_cfg)) =
        config.subagent_provider_for_model(Some(&subagent_model))
    else {
        return Err(anyhow!(
            "no configured provider matched subagent model '{subagent_model}'"
        ));
    };
    let api_base = config
        .agents
        .subagents
        .api_base
        .clone()
        .or_else(|| config.provider_api_base_for_provider(&provider_name));
    let provider = build_provider_client(
        &provider_name,
        &provider_cfg,
        &subagent_model,
        api_base,
        config.tools.web.proxy.as_deref(),
        config.agents.defaults.temperature,
    )?;

    if provider.default_model() == main_provider.default_model()
        && config.agents.subagents.model.trim().is_empty()
    {
        return Ok(BuiltSubagentProvider {
            provider: Some(provider),
            model: None,
        });
    }
    Ok(BuiltSubagentProvider {
        provider: Some(provider),
        model: Some(subagent_model),
    })
}

fn turn_summary(agent: &AgentLoop, elapsed: std::time::Duration) -> Result<TurnSummary> {
    let snapshot = agent.snapshot()?;
    Ok(TurnSummary {
        prompt_tokens: snapshot.last_prompt_tokens,
        completion_tokens: snapshot.last_completion_tokens,
        cached_tokens: snapshot.last_cached_tokens,
        elapsed,
    })
}

fn resolve_cli_workspace(
    config: &Config,
    cwd: &Path,
    workspace_override: Option<&Path>,
    use_global_workspace: bool,
) -> PathBuf {
    if let Some(workspace) = workspace_override {
        return expand_workspace_path(workspace);
    }
    if use_global_workspace {
        return config.workspace_path();
    }
    cwd.to_path_buf()
}

fn resolve_run_workspace(config: &Config, workspace_override: Option<&Path>) -> PathBuf {
    workspace_override
        .map(expand_workspace_path)
        .unwrap_or_else(|| config.workspace_path())
}

fn expand_workspace_path(path: &Path) -> PathBuf {
    expand_tilde(&path.as_os_str().to_string_lossy())
}

fn gateway_url(host: &str, port: u16, path: &str) -> String {
    format!(
        "http://{}:{port}{path}",
        gateway_display_host(host).as_str()
    )
}

fn format_gateway_url(url: &str) -> String {
    format!("{}", url.blue().bold())
}

fn format_max_context_length(total_tokens: usize) -> String {
    format!("{total_tokens} max")
}

fn format_run_welcome(
    config_path: &Path,
    workspace: &Path,
    config: &Config,
    model: &str,
    provider_name: &str,
    context_status: &str,
    enabled_channels: &[String],
    gateway_bind: &str,
    gateway_public: &str,
    admin_url: &str,
    metrics_url: &str,
) -> String {
    let rows = vec![
        ("mode", "run".to_string()),
        ("config", config_path.display().to_string()),
        ("running workspace", workspace.display().to_string()),
        ("model", model.to_string()),
        (
            "provider",
            format!(
                "{} ({})",
                provider_name,
                config
                    .provider_api_base_for_model(Some(model))
                    .unwrap_or_else(|| "default api base".to_string())
            ),
        ),
        (
            "agents",
            format!(
                "maxTokens={}  context={}\nmaxToolIterations={}  memory={}B",
                config.agents.defaults.max_tokens,
                config.agents.defaults.context_window_tokens,
                config.agents.defaults.max_tool_iterations,
                config.agents.defaults.memory_max_bytes
            ),
        ),
        ("context", context_status.to_string()),
        ("channels", summarize_run_channels(config, enabled_channels)),
        ("tools", summarize_run_tools(config)),
        (
            "gateway",
            format!("bind={}  public={}", gateway_bind, gateway_public),
        ),
        (
            "admin",
            if config.gateway.admin.enabled {
                admin_url.to_string()
            } else {
                "disabled".to_string()
            },
        ),
        (
            "metrics",
            if config.gateway.metrics.enabled {
                metrics_url.to_string()
            } else {
                "disabled".to_string()
            },
        ),
        (
            "heartbeat",
            if config.gateway.heartbeat.enabled {
                format!("enabled ({}s)", config.gateway.heartbeat.interval_s)
            } else {
                "disabled".to_string()
            },
        ),
        ("stop", "Press Ctrl-C to stop.".to_string()),
    ];
    format_left_rounded_kv_panel("xbot runtime", &rows)
}

fn summarize_run_channels(config: &Config, enabled_channels: &[String]) -> String {
    let enabled = if enabled_channels.is_empty() {
        "none".to_string()
    } else {
        enabled_channels.join(", ")
    };
    let tool_hints = if config.channels.send_tool_hints {
        "tool hints on"
    } else {
        "tool hints muted"
    };
    let progress = if config.channels.send_progress {
        "progress on"
    } else {
        "progress off"
    };
    if enabled_channels.is_empty() {
        format!("{enabled} ({progress}, {tool_hints}; gateway/admin still available)")
    } else {
        format!("{enabled} ({progress}, {tool_hints})")
    }
}

fn summarize_run_tools(config: &Config) -> String {
    let exec = if config.tools.exec.enable {
        format!("exec on/{}s", config.tools.exec.timeout)
    } else {
        "exec off".to_string()
    };
    let web_search = format!("web search {}", config.tools.web.search.provider);
    let mcp_enabled = config
        .tools
        .mcp_servers
        .iter()
        .filter(|(_, server)| server.enabled)
        .map(|(name, _)| name.as_str())
        .collect::<Vec<_>>();
    let mcp = if mcp_enabled.is_empty() {
        "mcp none".to_string()
    } else {
        format!("mcp {}", mcp_enabled.join(", "))
    };
    let scope = if config.tools.restrict_to_workspace {
        "workspace-only"
    } else {
        "workspace+system"
    };
    format!("{exec}  {web_search}\n{mcp}  {scope}")
}

fn format_left_rounded_kv_panel(title: &str, rows: &[(&str, String)]) -> String {
    let label_width = rows.iter().map(|(label, _)| label.len()).max().unwrap_or(0);
    let lines = rows
        .iter()
        .flat_map(|(label, value)| {
            let mut value_lines = value.lines();
            let first = value_lines
                .next()
                .map(|line| format!("  {} : {line}", format!("{label:label_width$}").bold()))
                .into_iter();
            let rest = value_lines.map(|line| format!("  {:label_width$}   {line}", ""));
            first.chain(rest).collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let top = format!("╭─ {}", title.cyan().bold());
    let body = lines
        .iter()
        .map(|line| format!("│ {line}"))
        .collect::<Vec<_>>();
    let bottom = "╰─".to_string();
    std::iter::once(top)
        .chain(body)
        .chain(std::iter::once(bottom))
        .collect::<Vec<_>>()
        .join("\n")
}

fn gateway_display_host(host: &str) -> String {
    resolve_gateway_display_host(host, detected_local_ip().as_deref())
}

fn resolve_gateway_display_host(host: &str, local_ip_hint: Option<&str>) -> String {
    if matches!(host, "0.0.0.0" | "::" | "[::]") {
        return local_ip_hint.unwrap_or("127.0.0.1").to_string();
    }
    host.to_string()
}

fn detected_local_ip() -> Option<String> {
    local_ip().ok().map(|ip| ip.to_string())
}

#[cfg(test)]
mod tests {
    use super::{
        format_run_welcome, onboard_command_prefix, onboard_config_command,
        repl_session_context_status, resolve_cli_workspace, resolve_gateway_display_host,
        resolve_run_workspace,
    };
    use anyhow::{Result, anyhow};
    use async_trait::async_trait;
    use serde_json::Value;
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::path::Path;
    use std::sync::Arc;
    use tempfile::tempdir;
    use xbot::config::Config;
    use xbot::engine::AgentLoop;
    use xbot::providers::{LlmProvider, LlmResponse, ProviderModelInfo};
    use xbot::storage::{ChatMessage, SessionManager};

    struct CatalogProvider {
        model: String,
        models: Vec<ProviderModelInfo>,
    }

    #[async_trait]
    impl LlmProvider for CatalogProvider {
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
            Err(anyhow!("unused in test"))
        }

        async fn list_models(&self) -> Result<Vec<ProviderModelInfo>> {
            Ok(self.models.clone())
        }
    }

    #[test]
    fn wildcard_gateway_host_uses_local_ip_hint() {
        assert_eq!(
            resolve_gateway_display_host("0.0.0.0", Some("192.168.1.25")),
            "192.168.1.25"
        );
        assert_eq!(
            resolve_gateway_display_host("::", Some("10.0.0.8")),
            "10.0.0.8"
        );
    }

    #[test]
    fn fixed_gateway_host_is_preserved() {
        assert_eq!(
            resolve_gateway_display_host("127.0.0.1", Some("192.168.1.25")),
            "127.0.0.1"
        );
        assert_eq!(
            resolve_gateway_display_host("example.local", Some("192.168.1.25")),
            "example.local"
        );
    }

    #[test]
    fn formats_run_welcome_header_with_runtime_summary() {
        let mut config = Config::default();
        config.agents.defaults.max_tokens = 16_384;
        config.agents.defaults.context_window_tokens = 32768;
        config.agents.defaults.max_tool_iterations = 12;
        config.agents.defaults.memory_max_bytes = 65536;
        config.channels.send_progress = true;
        config.channels.send_tool_hints = false;
        config.channels.sections = BTreeMap::from([
            (
                "slack".to_string(),
                json!({"enabled": true, "allowFrom": ["*"]}),
            ),
            (
                "telegram".to_string(),
                json!({"enabled": true, "allowFrom": ["*"]}),
            ),
        ]);
        config.tools.exec.enable = true;
        config.tools.exec.timeout = 90;
        config.tools.web.search.provider = "duckduckgo".to_string();
        config.tools.restrict_to_workspace = true;
        config.tools.mcp_servers.insert(
            "github".to_string(),
            xbot::config::McpServerConfig {
                enabled: true,
                ..Default::default()
            },
        );
        config.gateway.heartbeat.enabled = true;
        config.gateway.heartbeat.interval_s = 1800;

        let rendered = format_run_welcome(
            Path::new("/root/.xbot/config.json"),
            Path::new("/root/xbot"),
            &config,
            "openai/gpt-4.1-mini",
            "openai",
            "524288 max",
            &["slack".to_string(), "telegram".to_string()],
            "http://0.0.0.0:18790",
            "http://127.0.0.1:18790/",
            "http://127.0.0.1:18790/admin",
            "http://127.0.0.1:18790/metrics",
        );

        assert!(rendered.starts_with("╭─ xbot runtime"));
        assert!(rendered.contains("mode              : run"));
        assert!(rendered.contains("config            : /root/.xbot/config.json"));
        assert!(rendered.contains("running workspace : /root/xbot"));
        assert!(rendered.contains("model             : openai/gpt-4.1-mini"));
        assert!(rendered.contains("provider          : openai (default api base)"));
        assert!(rendered.contains("agents            : maxTokens=16384  context=32768"));
        assert!(rendered.contains("            maxToolIterations=12  memory=65536B"));
        assert!(rendered.contains("context           : 524288 max"));
        assert!(
            rendered
                .contains("channels          : slack, telegram (progress on, tool hints muted)")
        );
        assert!(rendered.contains("tools             : exec on/90s  web search duckduckgo"));
        assert!(rendered.contains("mcp github  workspace-only"));
        assert!(rendered.contains("gateway           : bind=http://0.0.0.0:18790"));
        assert!(rendered.contains("public=http://127.0.0.1:18790/"));
        assert!(rendered.contains("admin             : http://127.0.0.1:18790/admin"));
        assert!(rendered.contains("metrics           : http://127.0.0.1:18790/metrics"));
        assert!(rendered.contains("heartbeat         : enabled (1800s)"));
        assert!(rendered.contains("stop              : Press Ctrl-C to stop."));
        assert!(rendered.ends_with("╰─"));
    }

    #[test]
    fn cli_workspace_defaults_to_current_dir_without_existing_state() {
        let dir = tempdir().unwrap();
        let cwd = dir.path().join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let mut config = Config::default();
        config.agents.defaults.workspace = "/configured/workspace".to_string();

        assert_eq!(resolve_cli_workspace(&config, &cwd, None, false), cwd);
    }

    #[test]
    fn cli_workspace_uses_global_workspace_when_requested() {
        let dir = tempdir().unwrap();
        let cwd = dir.path().join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let configured = dir.path().join("configured");
        let mut config = Config::default();
        config.agents.defaults.workspace = configured.display().to_string();

        assert_eq!(resolve_cli_workspace(&config, &cwd, None, true), configured);
    }

    #[test]
    fn cli_workspace_uses_explicit_workspace_override() {
        let dir = tempdir().unwrap();
        let cwd = dir.path().join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let selected = dir.path().join("selected");
        let mut config = Config::default();
        config.agents.defaults.workspace = "/configured/workspace".to_string();

        assert_eq!(
            resolve_cli_workspace(&config, &cwd, Some(&selected), false),
            selected
        );
    }

    #[test]
    fn run_workspace_defaults_to_configured_workspace() {
        let dir = tempdir().unwrap();
        let configured = dir.path().join("configured");
        let mut config = Config::default();
        config.agents.defaults.workspace = configured.display().to_string();

        assert_eq!(resolve_run_workspace(&config, None), configured);
    }

    #[test]
    fn run_workspace_uses_explicit_workspace_override() {
        let dir = tempdir().unwrap();
        let configured = dir.path().join("configured");
        let selected = dir.path().join("selected");
        let mut config = Config::default();
        config.agents.defaults.workspace = configured.display().to_string();

        assert_eq!(resolve_run_workspace(&config, Some(&selected)), selected);
    }

    #[test]
    fn onboard_commands_match_cargo_release_invocation() {
        let prefix = onboard_command_prefix(Path::new("/repo/target/release/xbot"));

        assert_eq!(
            onboard_config_command(&prefix, None, "--provider"),
            "cargo run --release -- config --provider"
        );
        assert_eq!(
            onboard_config_command(&prefix, None, "--channel"),
            "cargo run --release -- config --channel"
        );
    }

    #[test]
    fn onboard_commands_match_installed_invocation_and_quote_config() {
        let prefix = onboard_command_prefix(Path::new("/usr/local/bin/xbot"));

        assert_eq!(prefix, "xbot");
        assert_eq!(
            onboard_config_command(
                &prefix,
                Some(Path::new("/tmp/my config.json")),
                "--provider"
            ),
            "xbot --config '/tmp/my config.json' config --provider"
        );
    }

    #[tokio::test]
    async fn repl_context_header_reuses_status_context_line() {
        let dir = tempdir().unwrap();
        let mut sessions = SessionManager::new(dir.path()).unwrap();
        let mut session = sessions.get_or_create("cli:demo").unwrap();
        session.add_message("user", "hello");
        session
            .metadata
            .insert("model".to_string(), Value::String("demo-model".to_string()));
        session
            .metadata
            .insert("contextTokens".to_string(), Value::from(5_u64));
        sessions.save(&session).unwrap();

        let agent = AgentLoop::new(
            Arc::new(CatalogProvider {
                model: "demo-model".to_string(),
                models: vec![ProviderModelInfo {
                    id: "demo-model".to_string(),
                    context_window_tokens: Some(524288),
                }],
            }),
            dir.path(),
            Some("demo-model".to_string()),
            8,
            5,
            262144,
            32 * 1024,
            Default::default(),
            None,
            xbot::config::ExecToolConfig {
                enable: false,
                timeout: 60,
                path_append: String::new(),
            },
            false,
            None,
            &Default::default(),
        )
        .await
        .unwrap();

        let context = repl_session_context_status(&agent, "cli:demo")
            .await
            .unwrap();
        assert_eq!(context, "5/524288 (0%)");
    }
}

fn cli_approval_callback(_stream: cli::StreamRenderer) -> xbot::tools::ApprovalCallback {
    use std::io::{IsTerminal, Write};
    use xbot::diff::DiffKind;
    use xbot::tools::{ApprovalDecision, ApprovalRequest};

    Arc::new(move |request: ApprovalRequest| {
        Box::pin(async move {
            let use_color = std::io::stdout().is_terminal();
            let mut output = String::new();

            let tool_icon = xbot::util::tool_emoji(&request.tool_name);
            if use_color {
                output.push_str(&format!(
                    "\n\x1b[1;33m╭─ {tool_icon} Approve {} ─╮\x1b[0m",
                    request.tool_name
                ));
                if let Some(source) = &request.source {
                    output.push_str(&format!(
                        "\n\x1b[33m│\x1b[0m  \x1b[36mFrom:\x1b[0m {source}"
                    ));
                }
                output.push_str(&format!(
                    "\n\x1b[33m│\x1b[0m  \x1b[36mFile:\x1b[0m {}",
                    request.path
                ));
                output.push_str(&format!("\n\x1b[33m├{}┤\x1b[0m", "─".repeat(40)));
            } else {
                output.push_str(&format!(
                    "\n╭─ {tool_icon} Approve {} ─╮",
                    request.tool_name
                ));
                if let Some(source) = &request.source {
                    output.push_str(&format!("\n│  From: {source}"));
                }
                output.push_str(&format!("\n│  File: {}", request.path));
                output.push_str(&format!("\n├{}┤", "─".repeat(40)));
            }

            for dl in request.diff_lines.iter().take(30) {
                let old_no = dl
                    .old_lineno
                    .map(|n| format!("{n:>4}"))
                    .unwrap_or_else(|| "    ".to_string());
                let new_no = dl
                    .new_lineno
                    .map(|n| format!("{n:>4}"))
                    .unwrap_or_else(|| "    ".to_string());
                let text = format!(" {} {} {} {}", old_no, new_no, dl.marker, dl.text);
                if use_color {
                    let bar = "\x1b[33m│\x1b[0m";
                    let colored = match dl.kind {
                        DiffKind::Added => format!("{bar}\x1b[32m{text}\x1b[0m"),
                        DiffKind::Removed => format!("{bar}\x1b[31m{text}\x1b[0m"),
                        DiffKind::Omitted => format!("{bar}\x1b[2m{text}\x1b[0m"),
                        DiffKind::Context => format!("{bar}{text}"),
                    };
                    output.push_str(&format!("\n{colored}"));
                } else {
                    output.push_str(&format!("\n│{text}"));
                }
            }
            if request.diff_lines.len() > 30 {
                let prefix = if use_color {
                    "\x1b[33m│\x1b[0m"
                } else {
                    "│"
                };
                output.push_str(&format!(
                    "\n{prefix}  … {} more lines",
                    request.diff_lines.len() - 30
                ));
            }

            if use_color {
                output.push_str(&format!("\n\x1b[33m╰{}╯\x1b[0m", "─".repeat(40)));
                output.push_str("\n");
                output.push_str("  \x1b[42;30m 1 Allow Once \x1b[0m");
                output.push_str("  \x1b[44;37m 2 Always Allow \x1b[0m");
                output.push_str("  \x1b[41;37m 3 Deny \x1b[0m");
                output.push_str("\n  \x1b[1;33m›\x1b[0m ");
            } else {
                output.push_str(&format!("\n╰{}╯", "─".repeat(40)));
                output.push_str("\n  [1] Allow Once  [2] Always Allow  [3] Deny\n  › ");
            }
            print!("{output}");
            let _ = std::io::stdout().flush();

            let decision = tokio::task::spawn_blocking(|| {
                let mut input = String::new();
                if std::io::stdin().read_line(&mut input).is_err() {
                    return ApprovalDecision::AllowOnce;
                }
                match input.trim() {
                    "2" | "a" | "always" => ApprovalDecision::AlwaysAllow,
                    "3" | "d" | "deny" | "n" => ApprovalDecision::Deny,
                    _ => ApprovalDecision::AllowOnce,
                }
            })
            .await
            .unwrap_or(ApprovalDecision::AllowOnce);

            decision
        })
    })
}

fn cli_progress_callback(stream: cli::StreamRenderer) -> MessageSendCallback {
    Arc::new(move |msg: OutboundMessage| {
        let stream = stream.clone();
        Box::pin(async move {
            if msg
                .metadata
                .get("_tool_hint")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
            {
                stream.tool_hint(
                    msg.content.trim(),
                    msg.metadata
                        .get("_tool_name")
                        .and_then(serde_json::Value::as_str),
                    msg.metadata.get("_tool_args"),
                );
                return Ok(());
            }
            if msg
                .metadata
                .get("_tool_result")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
            {
                let tool_name = msg
                    .metadata
                    .get("_tool_name")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("tool");
                let success = msg
                    .metadata
                    .get("_tool_success")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(true);
                let summary_text = msg
                    .metadata
                    .get("_tool_result_summary")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                stream.tool_result(tool_name, success, summary_text);
                return Ok(());
            }
            if let Some(event) = msg
                .metadata
                .get("_subagent_event")
                .and_then(serde_json::Value::as_str)
            {
                let label = msg
                    .metadata
                    .get("_subagent_label")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("subagent");
                stream.subagent_event(label, event);
                return Ok(());
            }
            Ok(())
        })
    })
}

fn cli_session_key(cwd: &Path) -> String {
    let mut hasher = DefaultHasher::new();
    cwd.to_string_lossy().hash(&mut hasher);
    let project = cwd
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "workspace".to_string());
    format!("cli:{project}:{:016x}", hasher.finish())
}

fn repl_session_message_count(workspace: &Path, session_key: &str) -> Result<usize> {
    let mut sessions = SessionManager::new(workspace)?;
    Ok(sessions.get_or_create(session_key)?.get_history(0).len())
}

fn repl_session_model(workspace: &Path, session_key: &str) -> Result<Option<String>> {
    let mut sessions = SessionManager::new(workspace)?;
    Ok(sessions
        .get_or_create(session_key)?
        .metadata
        .get("model")
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned))
}

async fn repl_session_context_status(agent: &AgentLoop, session_key: &str) -> Result<String> {
    let content = agent.session_status_content(session_key).await?;
    content
        .lines()
        .find_map(|line| line.strip_prefix("Context: ").map(ToOwned::to_owned))
        .ok_or_else(|| anyhow!("status content missing context line"))
}

fn build_heartbeat_service(
    config: &Config,
    workspace: &Path,
    bus: MessageBus,
    agent_slot: Arc<std::sync::Mutex<Option<Arc<AgentLoop>>>>,
    model: String,
    provider_name: String,
) -> Option<HeartbeatService> {
    if !config.gateway.heartbeat.enabled {
        return None;
    }

    let execute_agent_slot = agent_slot.clone();
    let execute =
        Arc::new(
            move |tasks: String| -> std::pin::Pin<
                Box<dyn std::future::Future<Output = Result<String>> + Send>,
            > {
                let execute_agent_slot = execute_agent_slot.clone();
                Box::pin(async move {
                    let Some(agent) = execute_agent_slot
                        .lock()
                        .expect("heartbeat agent slot lock poisoned")
                        .clone()
                    else {
                        return Ok(String::new());
                    };
                    let response = agent
                        .process_inbound(InboundMessage {
                            channel: "system".to_string(),
                            sender_id: "heartbeat".to_string(),
                            chat_id: "heartbeat:heartbeat".to_string(),
                            content: tasks,
                            timestamp: chrono::Utc::now(),
                            media: Vec::new(),
                            metadata: Default::default(),
                            session_key_override: None,
                        })
                        .await?;
                    Ok(response.map(|msg| msg.content).unwrap_or_default())
                })
            },
        );

    let notify_bus = bus.clone();
    let notify = Arc::new(
        move |response: String| -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<()>> + Send>,
        > {
            let notify_bus = notify_bus.clone();
            Box::pin(async move {
                if response.trim().is_empty() {
                return Ok(());
            }
            notify_bus
                .publish_outbound(OutboundMessage {
                    channel: "system".to_string(),
                    chat_id: "heartbeat".to_string(),
                    content: response,
                    reply_to: None,
                    media: Vec::new(),
                    reasoning_content: None,
                    metadata: Default::default(),
                })
                .await?;
                Ok(())
            })
        },
    );

    Some(HeartbeatService::new(
        workspace,
        build_provider_client(
            &provider_name,
            config.providers.get(&provider_name)?,
            &model,
            config.provider_api_base_for_model(Some(&model)),
            config.tools.web.proxy.as_deref(),
            config.agents.defaults.temperature,
        )
        .ok()?,
        model,
        Some(execute),
        Some(notify),
        None,
        config.gateway.heartbeat.interval_s,
        true,
    ))
}
