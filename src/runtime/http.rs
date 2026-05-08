use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use axum::body::{Body, to_bytes};
use axum::extract::{Path as AxumPath, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::{Json, Router};
use chrono::Utc;
use hmac::{Hmac, Mac};
use serde_json::{Value, json};
use sha2::Sha256;

use crate::channels::{Channel, ChannelManager, FeishuChannel, SlackChannel, TelegramChannel};
use crate::config::{ChannelsConfig, Config};
use crate::cron::CronService;
use crate::engine::AgentLoop;
use crate::observability::{
    RuntimeTelemetry, collect_provider_model_snapshot, collect_system_snapshot,
};
use crate::runtime::heartbeat::HeartbeatService;

#[derive(Clone)]
pub struct AdminSurface {
    pub agent: Arc<AgentLoop>,
    pub cron: CronService,
    pub heartbeat: Option<HeartbeatService>,
    pub telemetry: RuntimeTelemetry,
    pub config: Config,
    pub workspace: PathBuf,
}

#[derive(Clone)]
struct GatewayState {
    manager: Arc<ChannelManager>,
    config: Config,
    routes: Arc<BTreeMap<String, WebhookTarget>>,
    admin: Option<AdminSurface>,
}

#[derive(Clone)]
enum WebhookTarget {
    Slack {
        channel: Arc<dyn Channel>,
        signing_secret: String,
    },
    Telegram {
        channel: Arc<dyn Channel>,
        secret: String,
    },
    Feishu {
        channel: Arc<dyn Channel>,
        verification_token: String,
    },
}

fn normalize_path(path: &str) -> String {
    if path.trim().is_empty() {
        "/".to_string()
    } else if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    }
}

pub fn build_webhook_router(
    manager: &Arc<ChannelManager>,
    config: &ChannelsConfig,
) -> Result<Option<Router>> {
    let runtime_config = Config {
        channels: config.clone(),
        ..Config::default()
    };
    build_gateway_router(manager, &runtime_config, None, None, None, None)
}

pub fn build_gateway_router(
    manager: &Arc<ChannelManager>,
    config: &Config,
    agent: Option<Arc<AgentLoop>>,
    cron: Option<CronService>,
    heartbeat: Option<HeartbeatService>,
    telemetry: Option<RuntimeTelemetry>,
) -> Result<Option<Router>> {
    let mut routes = BTreeMap::new();
    if let Some(channel) = manager.get_channel("slack") {
        if channel.as_any().downcast_ref::<SlackChannel>().is_some() {
            let section = config.channels.section("slack");
            let mode = section
                .and_then(|section| section.get("mode"))
                .and_then(Value::as_str)
                .unwrap_or("webhook");
            if !mode.eq_ignore_ascii_case("socket") {
                let path = normalize_path(
                    section
                        .and_then(|section| section.get("webhookPath"))
                        .and_then(Value::as_str)
                        .unwrap_or("/slack/events"),
                );
                let signing_secret = section
                    .and_then(|section| section.get("signingSecret"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                insert_webhook_route(
                    &mut routes,
                    path,
                    WebhookTarget::Slack {
                        channel,
                        signing_secret,
                    },
                )?;
            }
        }
    }
    if let Some(channel) = manager.get_channel("telegram") {
        if channel.as_any().downcast_ref::<TelegramChannel>().is_some() {
            let section = config.channels.section("telegram");
            let path = normalize_path(
                section
                    .and_then(|section| section.get("webhookPath"))
                    .and_then(Value::as_str)
                    .unwrap_or("/telegram/webhook"),
            );
            let secret = section
                .and_then(|section| section.get("webhookSecret"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            insert_webhook_route(
                &mut routes,
                path,
                WebhookTarget::Telegram { channel, secret },
            )?;
        }
    }
    if let Some(channel) = manager.get_channel("feishu") {
        if channel.as_any().downcast_ref::<FeishuChannel>().is_some() {
            let section = config.channels.section("feishu");
            let path = normalize_path(
                section
                    .and_then(|section| section.get("webhookPath"))
                    .and_then(Value::as_str)
                    .unwrap_or("/feishu/events"),
            );
            let verification_token = section
                .and_then(|section| section.get("verificationToken"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            insert_webhook_route(
                &mut routes,
                path,
                WebhookTarget::Feishu {
                    channel,
                    verification_token,
                },
            )?;
        }
    }

    let admin = match (agent, cron, telemetry) {
        (Some(agent), Some(cron), Some(telemetry)) => Some(AdminSurface {
            workspace: config.workspace_path(),
            agent,
            cron,
            heartbeat,
            telemetry,
            config: config.clone(),
        }),
        _ => None,
    };

    let admin_path = normalize_path(&config.gateway.admin.path);
    let metrics_path = normalize_path(&config.gateway.metrics.path);
    Ok(Some(
        Router::new()
            .route("/healthz", get(healthz))
            .route("/readyz", get(readyz))
            .route("/status", get(status))
            .route("/api/admin/overview", get(admin_overview))
            .route("/api/admin/sessions", get(admin_sessions))
            .route("/api/admin/cron", get(admin_cron))
            .route("/api/admin/config", get(admin_config))
            .route(
                "/api/admin/heartbeat/trigger",
                post(admin_trigger_heartbeat),
            )
            .route(
                "/api/admin/channels/{name}/{action}",
                post(admin_channel_action),
            )
            .route(&admin_path, get(admin_ui))
            .route(&metrics_path, get(metrics))
            .route("/{*path}", any(handle_webhook))
            .with_state(GatewayState {
                manager: manager.clone(),
                config: config.clone(),
                routes: Arc::new(routes),
                admin,
            }),
    ))
}

fn insert_webhook_route(
    routes: &mut BTreeMap<String, WebhookTarget>,
    path: String,
    target: WebhookTarget,
) -> Result<()> {
    if routes.contains_key(&path) {
        anyhow::bail!("duplicate gateway route configured for '{path}'");
    }
    routes.insert(path, target);
    Ok(())
}

async fn healthz() -> Response {
    (StatusCode::OK, "ok").into_response()
}

async fn readyz(State(state): State<GatewayState>) -> Response {
    let status = state.manager.status();
    let ready = !status.is_empty()
        && status
            .values()
            .all(|entry| *entry.get("running").unwrap_or(&false));
    if ready {
        (StatusCode::OK, "ready").into_response()
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "not ready").into_response()
    }
}

async fn status(State(state): State<GatewayState>) -> Response {
    Json(json!({
        "channels": state.manager.status(),
        "webhooks": state.routes.keys().cloned().collect::<Vec<_>>(),
    }))
    .into_response()
}

async fn admin_ui(State(state): State<GatewayState>) -> Response {
    if state.admin.is_none() || !state.config.gateway.admin.enabled {
        return (StatusCode::NOT_FOUND, "admin UI disabled").into_response();
    }
    Html(admin_html(&state.config.gateway.metrics.path)).into_response()
}

async fn metrics(State(state): State<GatewayState>) -> Response {
    let Some(admin) = &state.admin else {
        return (StatusCode::NOT_FOUND, "metrics unavailable").into_response();
    };
    if !state.config.gateway.metrics.enabled {
        return (StatusCode::NOT_FOUND, "metrics disabled").into_response();
    }
    (
        StatusCode::OK,
        [("content-type", "text/plain; version=0.0.4; charset=utf-8")],
        admin.telemetry.render_prometheus(),
    )
        .into_response()
}

async fn admin_overview(State(state): State<GatewayState>) -> Response {
    let Some(admin) = &state.admin else {
        return (StatusCode::NOT_FOUND, "admin API unavailable").into_response();
    };
    let agent_snapshot = match admin.agent.snapshot() {
        Ok(snapshot) => snapshot,
        Err(err) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to collect agent snapshot: {err}"),
            )
                .into_response();
        }
    };
    let system = collect_system_snapshot().await;
    let provider_name = state
        .config
        .provider_name_for_model(Some(&agent_snapshot.model))
        .unwrap_or_else(|| "unknown".to_string());
    let api_key = state
        .config
        .provider_for_model(Some(&agent_snapshot.model))
        .map(|(_, cfg)| cfg.api_key)
        .filter(|k| !k.trim().is_empty());
    let provider = collect_provider_model_snapshot(
        &provider_name,
        &agent_snapshot.model,
        state
            .config
            .provider_api_base_for_model(Some(&agent_snapshot.model))
            .as_deref(),
        api_key.as_deref(),
    )
    .await;
    let cron_status = admin.cron.status().ok();
    Json(json!({
        "runtime": admin.telemetry.snapshot(),
        "agent": agent_snapshot,
        "system": system,
        "provider": provider,
        "channels": state.manager.status(),
        "cronStatus": cron_status.map(|(running, jobs, next_run)| json!({
            "running": running,
            "jobs": jobs,
            "nextRunAtMs": next_run,
        })),
        "heartbeat": admin.heartbeat.as_ref().map(|service| json!({
            "enabled": true,
            "running": service.is_running(),
            "file": service.heartbeat_file(),
        })).unwrap_or_else(|| json!({"enabled": false})),
        "workspace": admin.workspace,
    }))
    .into_response()
}

async fn admin_sessions(State(state): State<GatewayState>) -> Response {
    let Some(admin) = &state.admin else {
        return (StatusCode::NOT_FOUND, "admin API unavailable").into_response();
    };
    match admin.agent.session_summaries() {
        Ok(summaries) => Json(json!({"sessions": summaries})).into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to list sessions: {err}"),
        )
            .into_response(),
    }
}

async fn admin_cron(State(state): State<GatewayState>) -> Response {
    let Some(admin) = &state.admin else {
        return (StatusCode::NOT_FOUND, "admin API unavailable").into_response();
    };
    match admin.cron.list_jobs(true) {
        Ok(jobs) => Json(json!({"jobs": jobs})).into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to list cron jobs: {err}"),
        )
            .into_response(),
    }
}

async fn admin_config(State(state): State<GatewayState>) -> Response {
    let Some(admin) = &state.admin else {
        return (StatusCode::NOT_FOUND, "admin API unavailable").into_response();
    };
    match serde_json::to_value(&admin.config) {
        Ok(value) => Json(redact_secrets(value)).into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to serialize config: {err}"),
        )
            .into_response(),
    }
}

async fn admin_trigger_heartbeat(State(state): State<GatewayState>) -> Response {
    let Some(admin) = &state.admin else {
        return (StatusCode::NOT_FOUND, "admin API unavailable").into_response();
    };
    let Some(heartbeat) = &admin.heartbeat else {
        return (StatusCode::BAD_REQUEST, "heartbeat is disabled").into_response();
    };
    match heartbeat.trigger_now().await {
        Ok(result) => Json(json!({"result": result})).into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to trigger heartbeat: {err}"),
        )
            .into_response(),
    }
}

async fn admin_channel_action(
    State(state): State<GatewayState>,
    AxumPath((name, action)): AxumPath<(String, String)>,
) -> Response {
    let Some(_admin) = &state.admin else {
        return (StatusCode::NOT_FOUND, "admin API unavailable").into_response();
    };
    let result = match action.as_str() {
        "start" => state.manager.start_channel(&name).await,
        "stop" => state.manager.stop_channel(&name).await,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                "unsupported action; use start or stop",
            )
                .into_response();
        }
    };
    match result {
        Ok(()) => Json(json!({"ok": true, "channel": name, "action": action})).into_response(),
        Err(err) => (StatusCode::BAD_REQUEST, err.to_string()).into_response(),
    }
}

async fn handle_webhook(State(state): State<GatewayState>, req: Request<Body>) -> Response {
    let path = normalize_path(req.uri().path());
    if matches!(
        path.as_str(),
        "/healthz"
            | "/readyz"
            | "/status"
            | "/api/admin/overview"
            | "/api/admin/sessions"
            | "/api/admin/cron"
            | "/api/admin/config"
            | "/api/admin/heartbeat/trigger"
    ) || path == normalize_path(&state.config.gateway.admin.path)
        || path == normalize_path(&state.config.gateway.metrics.path)
        || path.starts_with("/api/admin/channels/")
    {
        return (StatusCode::METHOD_NOT_ALLOWED, "method not allowed").into_response();
    }
    let Some(target) = state.routes.get(&path).cloned() else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    let headers = req.headers().clone();
    let body = match to_bytes(req.into_body(), 2 * 1024 * 1024).await {
        Ok(body) => body,
        Err(err) => {
            return (
                StatusCode::BAD_REQUEST,
                format!("failed to read request body: {err}"),
            )
                .into_response();
        }
    };
    let payload = match serde_json::from_slice::<Value>(&body) {
        Ok(payload) => payload,
        Err(err) => {
            return (StatusCode::BAD_REQUEST, format!("invalid JSON: {err}")).into_response();
        }
    };

    match dispatch_webhook(target, &headers, &body, payload).await {
        Ok(response) => response,
        Err(err) => (StatusCode::BAD_REQUEST, err.to_string()).into_response(),
    }
}

async fn dispatch_webhook(
    target: WebhookTarget,
    headers: &HeaderMap,
    raw_body: &[u8],
    payload: Value,
) -> Result<Response> {
    match target {
        WebhookTarget::Slack {
            channel,
            signing_secret,
        } => {
            if !verify_slack_request(&signing_secret, headers, raw_body)? {
                eprintln!("[slack] rejected webhook: invalid signature");
                return Ok((StatusCode::UNAUTHORIZED, "invalid slack signature").into_response());
            }
            if payload.get("type").and_then(Value::as_str) == Some("url_verification") {
                let challenge = payload
                    .get("challenge")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                eprintln!("[slack] received url_verification challenge");
                return Ok((StatusCode::OK, challenge).into_response());
            }
            if let Some(event) = payload.get("event") {
                eprintln!(
                    "[slack] webhook received event type '{}'",
                    event
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                );
                let slack = channel
                    .as_any()
                    .downcast_ref::<SlackChannel>()
                    .ok_or_else(|| anyhow::anyhow!("invalid slack channel target"))?;
                slack.handle_event(event).await?;
            }
            Ok((StatusCode::OK, "ok").into_response())
        }
        WebhookTarget::Telegram { channel, secret } => {
            if !secret.is_empty() {
                let provided = headers
                    .get("x-telegram-bot-api-secret-token")
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or_default();
                if provided != secret {
                    return Ok(
                        (StatusCode::UNAUTHORIZED, "invalid telegram secret").into_response()
                    );
                }
            }
            let telegram = channel
                .as_any()
                .downcast_ref::<TelegramChannel>()
                .ok_or_else(|| anyhow::anyhow!("invalid telegram channel target"))?;
            telegram.handle_update(&payload).await?;
            Ok((StatusCode::OK, "ok").into_response())
        }
        WebhookTarget::Feishu {
            channel,
            verification_token,
        } => {
            if !verification_token.is_empty() {
                let token = payload
                    .get("token")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if !token.is_empty() && token != verification_token {
                    return Ok((
                        StatusCode::UNAUTHORIZED,
                        "invalid feishu verification token",
                    )
                        .into_response());
                }
            }
            if payload.get("challenge").is_some() {
                return Ok(Json(json!({
                    "challenge": payload.get("challenge").cloned().unwrap_or(Value::Null)
                }))
                .into_response());
            }
            let feishu = channel
                .as_any()
                .downcast_ref::<FeishuChannel>()
                .ok_or_else(|| anyhow::anyhow!("invalid feishu channel target"))?;
            feishu.handle_event(&payload).await?;
            Ok((StatusCode::OK, "ok").into_response())
        }
    }
}

fn verify_slack_request(secret: &str, headers: &HeaderMap, raw_body: &[u8]) -> Result<bool> {
    if secret.trim().is_empty() {
        return Ok(true);
    }
    let Some(timestamp) = headers
        .get("x-slack-request-timestamp")
        .and_then(|value| value.to_str().ok())
    else {
        return Ok(false);
    };
    let Ok(timestamp) = timestamp.parse::<i64>() else {
        return Ok(false);
    };
    let now = Utc::now().timestamp();
    if (now - timestamp).abs() > Duration::from_secs(300).as_secs() as i64 {
        return Ok(false);
    }
    let Some(signature) = headers
        .get("x-slack-signature")
        .and_then(|value| value.to_str().ok())
    else {
        return Ok(false);
    };
    let payload = format!("v0:{timestamp}:{}", String::from_utf8_lossy(raw_body));
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())?;
    mac.update(payload.as_bytes());
    let expected = format!("v0={}", hex::encode(mac.finalize().into_bytes()));
    Ok(expected == signature)
}

fn redact_secrets(value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(key, value)| {
                    let lowered = key.to_ascii_lowercase();
                    let redacted = if [
                        "apikey",
                        "api_key",
                        "secret",
                        "password",
                        "token",
                        "appsecret",
                        "signingsecret",
                    ]
                    .iter()
                    .any(|needle| lowered.contains(needle))
                    {
                        Value::String("[redacted]".to_string())
                    } else {
                        redact_secrets(value)
                    };
                    (key, redacted)
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.into_iter().map(redact_secrets).collect()),
        other => other,
    }
}

fn admin_html(metrics_path: &str) -> String {
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width,initial-scale=1">
  <title>rbot Admin</title>
  <style>
    :root {{
      --bg: #0f172a;
      --panel: #111827;
      --panel-2: #1f2937;
      --text: #e5e7eb;
      --muted: #94a3b8;
      --accent: #22c55e;
      --warn: #f59e0b;
      --danger: #ef4444;
      --border: rgba(148,163,184,0.2);
    }}
    body {{ margin:0; font-family: "SF Mono", ui-monospace, Menlo, monospace; background: radial-gradient(circle at top, #1e293b, var(--bg)); color: var(--text); }}
    header {{ padding: 24px 28px 12px; }}
    h1 {{ margin: 0; font-size: 28px; }}
    main {{ padding: 16px 28px 36px; display: grid; gap: 16px; }}
    .grid {{ display:grid; gap:16px; grid-template-columns: repeat(auto-fit, minmax(280px, 1fr)); }}
    .card {{ background: rgba(17,24,39,0.88); border:1px solid var(--border); border-radius: 16px; padding: 16px; box-shadow: 0 12px 40px rgba(0,0,0,0.25); }}
    h2 {{ margin:0 0 12px; font-size: 14px; text-transform: uppercase; letter-spacing: .12em; color: var(--muted); }}
    dl {{ display:grid; grid-template-columns: auto 1fr; gap: 8px 12px; margin:0; }}
    dt {{ color: var(--muted); }}
    dd {{ margin:0; word-break: break-word; }}
    table {{ width:100%; border-collapse: collapse; font-size: 13px; }}
    th, td {{ padding: 8px 6px; border-bottom: 1px solid var(--border); text-align: left; }}
    .actions {{ display:flex; gap:8px; flex-wrap: wrap; }}
    button {{ background: var(--panel-2); color: var(--text); border:1px solid var(--border); border-radius:999px; padding:8px 12px; cursor:pointer; }}
    button:hover {{ border-color: var(--accent); }}
    pre {{ margin:0; white-space: pre-wrap; word-break: break-word; color: #cbd5e1; }}
    .muted {{ color: var(--muted); }}
    a {{ color: #7dd3fc; }}
  </style>
</head>
<body>
  <header>
    <h1>rbot Admin</h1>
    <div class="muted">Operations console for runtime status, sessions, channels, and model telemetry.</div>
  </header>
  <main>
    <div class="grid">
      <section class="card"><h2>Runtime</h2><dl id="runtime"></dl></section>
      <section class="card"><h2>Provider</h2><dl id="provider"></dl></section>
      <section class="card"><h2>System</h2><dl id="system"></dl></section>
      <section class="card"><h2>Actions</h2><div class="actions"><button onclick="triggerHeartbeat()">Trigger Heartbeat</button><a href="{metrics_path}" target="_blank" rel="noreferrer">Open Metrics</a></div><pre id="actionResult" class="muted"></pre></section>
    </div>
    <section class="card"><h2>Channels</h2><table><thead><tr><th>Name</th><th>Enabled</th><th>Running</th><th>Action</th></tr></thead><tbody id="channels"></tbody></table></section>
    <section class="card"><h2>Sessions</h2><table><thead><tr><th>Session</th><th>Updated</th><th>Messages</th><th>Consolidated</th></tr></thead><tbody id="sessions"></tbody></table></section>
    <section class="card"><h2>Cron Jobs</h2><table><thead><tr><th>Name</th><th>Enabled</th><th>Next Run</th><th>Last Status</th></tr></thead><tbody id="jobs"></tbody></table></section>
  </main>
  <script>
    async function fetchJson(url, options) {{
      const res = await fetch(url, options);
      if (!res.ok) throw new Error(await res.text());
      return await res.json();
    }}
    function setDl(id, entries) {{
      const root = document.getElementById(id);
      root.innerHTML = entries.map(([k, v]) => `<dt>${{k}}</dt><dd>${{v ?? "n/a"}}</dd>`).join("");
    }}
    async function refresh() {{
      const [overview, sessions, cron] = await Promise.all([
        fetchJson('/api/admin/overview'),
        fetchJson('/api/admin/sessions'),
        fetchJson('/api/admin/cron')
      ]);
      setDl('runtime', [
        ['Uptime (s)', overview.runtime.uptime_seconds],
        ['Inbound', overview.runtime.inbound_messages],
        ['Outbound', overview.runtime.outbound_messages],
        ['Model', overview.agent.model],
        ['Sessions', overview.agent.session_count],
        ['Subagents', overview.agent.running_subagents],
        ['Prompt tokens', overview.runtime.provider.prompt_tokens],
        ['Completion tokens', overview.runtime.provider.completion_tokens]
      ]);
      setDl('provider', [
        ['Provider', overview.provider.provider_name],
        ['Model id', overview.provider.model_id],
        ['API base', overview.provider.api_base],
        ['Model path', overview.provider.model_path],
        ['Model size (bytes)', overview.provider.model_size_bytes],
        ['Avg latency ms', overview.runtime.provider.avg_latency_ms.toFixed(2)],
        ['Avg prefill tok/s', overview.runtime.provider.avg_prefill_tokens_per_s.toFixed(2)],
        ['Avg gen tok/s', overview.runtime.provider.avg_generation_tokens_per_s.toFixed(2)],
        ['Failures', overview.runtime.provider.failures]
      ]);
      const gpu = (overview.system.gpus || []).map(g => `${{g.name}} (${{g.utilization_pct ?? "?"}}% / ${{g.memory_used_mb ?? "?"}}MB)`).join('<br>') || 'n/a';
      setDl('system', [
        ['Host', overview.system.host_name],
        ['OS', overview.system.os_name],
        ['Kernel', overview.system.kernel_version],
        ['CPU usage %', overview.system.cpu_usage_pct.toFixed(2)],
        ['Memory', `${{overview.system.used_memory_bytes}} / ${{overview.system.total_memory_bytes}}`],
        ['Processes', overview.system.process_count],
        ['GPU', gpu]
      ]);
      document.getElementById('channels').innerHTML = Object.entries(overview.channels).map(([name, info]) => `
        <tr>
          <td>${{name}}</td>
          <td>${{info.enabled}}</td>
          <td>${{info.running}}</td>
          <td><button onclick="channelAction('${{name}}','${{info.running ? 'stop' : 'start'}}')">${{info.running ? 'Stop' : 'Start'}}</button></td>
        </tr>`).join('');
      document.getElementById('sessions').innerHTML = sessions.sessions.map(item => `
        <tr><td>${{item.key}}</td><td>${{item.updated_at}}</td><td>${{item.message_count}}</td><td>${{item.last_consolidated}}</td></tr>
      `).join('');
      document.getElementById('jobs').innerHTML = cron.jobs.map(item => `
        <tr><td>${{item.name}}</td><td>${{item.enabled}}</td><td>${{item.state.next_run_at_ms ?? ''}}</td><td>${{item.state.last_status ?? ''}}</td></tr>
      `).join('');
    }}
    async function triggerHeartbeat() {{
      try {{
        const result = await fetchJson('/api/admin/heartbeat/trigger', {{ method: 'POST' }});
        document.getElementById('actionResult').textContent = JSON.stringify(result, null, 2);
      }} catch (err) {{
        document.getElementById('actionResult').textContent = String(err);
      }}
    }}
    async function channelAction(name, action) {{
      try {{
        const result = await fetchJson(`/api/admin/channels/${{name}}/${{action}}`, {{ method: 'POST' }});
        document.getElementById('actionResult').textContent = JSON.stringify(result, null, 2);
        await refresh();
      }} catch (err) {{
        document.getElementById('actionResult').textContent = String(err);
      }}
    }}
    refresh();
    setInterval(refresh, 5000);
  </script>
</body>
</html>"#
    )
}
