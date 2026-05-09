# xbot Operations Guide

## Product Modes

`xbot` is designed to cover three persistent use cases from the same runtime:

- AI assistant: interactive support over `email`, `slack`, `telegram`, `feishu`, `dingtalk`, `discord`, `matrix`, `whatsapp`, `qq`, `wecom`, `weixin`, or `mochat`
- autonomous software engineer: file edits, shell execution, tests, scheduled repo checks, GitHub/CI assistance through skills and MCP tools
- autonomous data analyst: web search, web fetch, scheduled reports, workspace report generation, and channel delivery

## Core Building Blocks

The runtime already includes the main components required for unattended operation:

- persistent sessions
- long-term memory files
- shell/filesystem/web tools
- cron scheduling
- heartbeat review
- background subagents
- built-in skills
- OpenAI-compatible local or remote providers
- admin API/UI and metrics

Subagents:

- By default, subagents inherit the main task provider and model.
- Set `agents.subagents.model` and `agents.subagents.provider` to run subagents on a cheaper/faster model.
- Set `agents.subagents.apiBase`, or the matching `providers.<name>.apiBase`, to point subagents at a different OpenAI-compatible API server.
- If `agents.subagents.model` is empty, subagents keep using the main task model.

Memory behavior:

- `MEMORY.md` is permanent memory and is trimmed to `agents.defaults.memoryMaxBytes`
- finished user tasks are summarized into `MEMORY.md`
- `memorize` / `/memorize <text>` writes user-directed durable memory
- `clear` / `/clear` restores `HISTORY.md` to the default template for the workspace

## Built-in Skills

Built-in skills live under `xbot/skills/` and are loaded by the runtime automatically.

Recommended built-ins:

- `workspace-operator`
- `software-engineer`
- `data-analyst`
- `github-cli`
- `github`
- `scheduled-ops`
- `memory` (always-on)
- `memory-hygiene` (always-on)
- `cron`
- `clawhub`
- `skill-creator`
- `summarize`
- `weather`
- `tmux`

The runtime injects always-on skills automatically and adds task-relevant skills when prompt keywords match their trigger metadata. Skills with unmet requirements (missing binaries, environment variables, or OS constraints) are marked as unavailable in the skills summary.

### Skill Management CLI

```bash
xbot skills list          # List all skills with availability status
xbot skills init my-skill # Scaffold a new skill directory
```

## MCP for External Tooling

Use MCP when the built-in toolset is not enough.

Typical use cases:

- issue trackers
- browser automation
- database access
- internal APIs
- specialized data systems

Current support in `xbot`:

- MCP `stdio` transport
- startup validation for enabled servers
- MCP tools registered as normal native tools

Example:

```json
{
  "tools": {
    "mcpServers": {
      "browser": {
        "enabled": true,
        "type": "stdio",
        "command": "npx",
        "args": ["-y", "@modelcontextprotocol/server-playwright"],
        "enabledTools": ["*"],
        "toolTimeout": 45
      }
    }
  }
}
```

## Admin UI

When `xbot run` is active, the admin UI is available at:

- `http://<host>:<port>/admin`

It shows:

- runtime uptime and message counts
- provider request counts and token totals
- average provider latency
- average prompt and generation throughput
- model identity, discovered model metadata, and known local model inventory
- CPU, memory, process, and best-effort GPU usage
- channel state
- session summaries
- cron jobs

Supported actions:

- start a channel
- stop a channel
- trigger heartbeat immediately

## Metrics

The Prometheus endpoint is available at:

- `http://<host>:<port>/metrics`

Current metrics include:

- inbound and outbound message counters
- provider request, success, and failure counters
- total prompt and completion tokens
- average provider latency
- average prompt throughput
- average generation throughput

## CLI Operations

Useful commands:

```bash
cargo run -- status            # Runtime status
cargo run -- sessions          # List active sessions
cargo run -- jobs              # List scheduled cron jobs
cargo run -- print-config      # Print resolved config
cargo run -- channels list     # List all available channels
cargo run -- channels status   # Show enabled/disabled state per channel
cargo run -- channels login    # Interactive login (Weixin QR code, WhatsApp bridge)
cargo run -- channels setup    # Show setup instructions (how to get tokens/keys)
cargo run -- skills list       # List skills with availability status
cargo run -- skills init NAME  # Scaffold a new skill directory
cargo run -- config --provider # Interactive provider setup
cargo run -- config --channel  # Interactive channel setup
```

`status` resolves the current model/provider, inspects local system state, and prints the admin and metrics URLs.

## 24/7 Deployment Pattern

Recommended production pattern:

1. Use a stable workspace path under `~/.xbot/workspace` or a dedicated project directory.
2. Use a process supervisor such as `systemd`, `launchd`, Docker, or Kubernetes.
3. Point webhook-based channels at a stable public URL.
4. Use a local provider such as Ollama or vLLM for long-running internal workloads when appropriate.
5. Expose `/metrics` to your monitoring stack.
6. Review `.xbot/HEARTBEAT.md` and cron jobs regularly so unattended work stays bounded.

## Software Engineering Workflows

Recommended pattern:

1. Put repository-specific constraints in `.xbot/XBOT.md`, `.xbot/TOOLS.md`, and workspace-local skills.
2. Use the built-in `software-engineer` and `github-cli` skills.
3. Add MCP servers for systems the bot needs but cannot reach with the default tools.
4. Schedule repository health checks or report generation with cron.
5. Review the admin UI for queue pressure, failures, and token usage.

## Data Analysis Workflows

Recommended pattern:

1. Use the built-in `data-analyst` skill.
2. Save recurring reports into the workspace with timestamped filenames.
3. Schedule recurring collection and report jobs with cron.
4. Deliver summaries through channels and keep the detailed artifacts on disk.

## Concurrency and Safety

- `max_concurrent_requests` (default 3) limits total concurrent inbound processing via a global semaphore.
- Per-session mutex ensures messages for the same session are serialized, preventing interleaved tool execution.
- Channel outbound delivery retries with exponential backoff (`send_max_retries`, default 3).

## Reliability Notes

- Provider retries now only apply to transient failures.
- Local providers can run without API keys when recognized as local.
- OAuth providers (GitHub Copilot, OpenAI Codex) skip the API key requirement.
- Cursor provider requires explicit `apiBase` configuration.
- MCP configuration errors fail fast at startup.
- The admin API redacts secrets from the exposed config payload.
- Durable memory writes use the `memory-entry-writer` skill plus a short model summarization pass, with heuristic fallback if the provider summary fails.
- LLM-driven memory consolidation falls back to raw archive after 3 consecutive failures.
