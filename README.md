<div align="center">
  <img src="docs/logo.png" alt="xbot logo" width="180" style="margin-bottom: 0;" />
  <p style="margin: 0;">
    A Rust-native autonomous bot runtime for persistent task automation, vibe coding, and multi-channel message delivery. 🚀
  </p>
</div>

<p align="center">
  <a href="./README.md">English</a> |
  <a href="./README-CN.md">简体中文</a> |
</p>

## ✨ Features

- 🧠 **Persistent Agent Runtime** - Long-running agent runtime with persistent sessions, per-session serialization, and configurable concurrency control
- 📝 **Permanent Memory Capture** - LLM-driven memory consolidation, automatic task summaries, explicit `/memorize` support, and topic-relevant memory lookup
- 🛠️ **Rich Toolset** - Filesystem, shell, web fetch, web search, messaging, cron, and background-task tools
- 🌐 **Provider Integration** - OpenAI-compatible, Anthropic, GitHub Copilot (OAuth), Cursor, and local engines
- 🧵 **Hybrid Model Routing** - Run the main task on a remote frontier API such as DeepSeek `deepseek-v4-pro`, while background subagents use a local Qwen/vLLM/Ollama model for fast parallel work
- 🔌 **MCP Support** - MCP stdio tool integration for external tool servers
- 🧩 **Built-in Skills** - Software engineering, research/reporting, GitHub/CI, scheduled operations, memory management, cron, and clawhub marketplace
- 📬 **Multi-Channel** - 13 channel backends: `email`, `slack`, `telegram`, `feishu`, `dingtalk`, `discord`, `matrix`, `whatsapp`, `qq`, `wecom`, `weixin`, `mochat`, and extensible plugin channels
- 🌐 **Gateway Process** - Webhook ingress, health checks, readiness checks, Prometheus metrics, and a web admin UI
- 🔄 **Streaming** - Stream delta support with per-channel streaming, retry logic with exponential backoff
- 🪝 **Hook System** - Extensible `AgentHook` trait for lifecycle callbacks without modifying the core agent loop

## Overview (Hybrid Model Routing)
<img src="docs/xbot.png" alt="xbot terminal" width="600">

The screenshot highlights one of `xbot`'s core advantages: the main agent can use a remote high-capability model, while subagents fan out onto a separate local model. This lets you reserve paid remote tokens for synthesis and hard reasoning, and spend local GPU capacity on parallel exploration, code reading, tests, and report gathering.

## 📚 Documentation

**[📖 Full Documentation Website →](https://guoqingbao.github.io/xbot/)**

- [🚀 Getting Started](./docs/USAGE.md)
- [📦 Installation](./docs/INSTALLATION.md)
- [🧵 Hybrid Remote Main + Local Subagents](./docs/HYBRID_MODELS.md)
- [🏗️ Architecture](./docs/ARCHITECTURE.md)
- [⚙️ Operations Guide](./docs/OPERATIONS.md)
- [🔒 Security: SSRF Protection](#ssrf-protection)

## 🔒 Security: SSRF Protection

xbot includes Server-Side Request Forgery (SSRF) protection to prevent unauthorized access to internal networks when fetching URLs.

### Configuration

Add allowed private network ranges to your `~/.xbot/config.json`:

```json
{
  "tools": {
    "ssrfWhitelist": [
      "10.0.0.0/8",
      "192.168.0.0/16",
      "172.16.0.0/12"
    ]
  }
}
```

### How It Works

- **Default **(Strict) Empty or missing `ssrfWhitelist` blocks ALL private IP addresses
- **Whitelisted Networks**: Only explicitly listed CIDR ranges are allowed  
- **Applied Globally**: Protection applies to all agent tools:
  - `web_fetch` tool (used by LLM agents)
  - Discord/Telegram media uploads
  - Exec tool command arguments
- **TUI `/fetch` Command**: No SSRF protection - human-driven command allows any URL

### Security Notes

The TUI `/fetch` command intentionally has no SSRF protection because:
- It's directly controlled by human operators, not LLM agents
- Users need full freedom to fetch internal resources for debugging
- The risk profile is different from automated agent tools

For agent-triggered URL fetching, SSRF protection remains active and configurable via the global whitelist.

### Security Examples

**Block all private networks **(default)
```json
{
  "tools": {
    "ssrfWhitelist": []
  }
}
```

**Allow OpenStack private VLANs**:
```json
{
  "tools": {
    "ssrfWhitelist": [
      "10.0.0.0/8",
      "172.16.0.0/12",
      "192.168.0.0/16"
    ]
  }
}
```

**Allow Kubernetes cluster networks**:
```json
{
  "tools": {
    "ssrfWhitelist": [
      "10.96.0.0/12",
      "172.17.0.0/16"
    ]
  }
}
```

### Validation

- ✅ Supports CIDR notation (e.g., `192.168.0.0/16`)
- ✅ IPv4 and IPv6 ranges
- ✅ Invalid ranges are silently ignored
- ✅ DNS resolution happens before IP checking
- ✅ Redirect targets are re-validated

### Blocked Ranges

By default, SSRF protection blocks these private ranges (unless whitelisted):

- `0.0.0.0/8` - Invalid source addresses
- `10.0.0.0/8` - Private VLAN
- `100.64.0.0/10` - Carrier-Shared Addresses (CGNAT)
- `127.0.0.0/8` - Loopback
- `169.254.0.0/16` - Link-local
- `172.16.0.0/12` - RFC 1918 private
- `192.168.0.0/16` - RFC 1918 private
- `::1/128` - IPv6 loopback
- `fc00::/7` - IPv6 unique local
- `fe80::/10` - IPv6 link-local

## 🚀 Quick Start

### 📦 Install

**Option 1 — One-line install (Recommended)**

```bash
curl -sSL https://guoqingbao.github.io/xbot/install.sh | bash
```

Auto-detects your OS (Linux/macOS) and architecture (x64/ARM64), then installs the latest pre-built binary. On Linux, offers deb package or binary install. On macOS, installs to `/usr/local/bin`.

**Option 2 — npm**

```bash
npm install -g @trusted-ai/xbot
```

**Option 3 — Cargo**

```bash
cargo install xbot
```

**Option 4 — Debian/Ubuntu (.deb)**

```bash
# Download from GitHub Releases (choose your arch)
curl -fSL -o xbot.deb https://github.com/guoqingbao/xbot/releases/latest/download/xbot-linux-x64.deb
sudo dpkg -i xbot.deb
```

**Option 5 — Build from source**

```bash
git clone https://github.com/guoqingbao/xbot.git
cd xbot
cargo install --path .
```

The installed command is `xbot`. See [Installation](./docs/INSTALLATION.md) for details.

### Supported Platforms

| Platform | Architecture | Package Format |
|----------|-------------|----------------|
| Linux | x86_64 | .deb, .tar.gz |
| Linux | ARM64 | .deb, .tar.gz |
| macOS | Apple Silicon | .tar.gz |
| macOS | Intel x64 | .tar.gz |
| Windows | x86_64 | .zip |
| Windows | ARM64 | .zip |

### Initialize config and workspace:

```bash
xbot onboard
```

This will generate:

```python
# Global config file
Config: ~/.xbot/config.json
# Global workspace
Workspace: ~/.xbot/workspace
```

### Config Providers

`xbot` supports both remote and local OpenAI-compatible backends. 🎯
You can configure them interactively:

```bash
xbot config --provider
```

Or manually edit `~/.xbot/config.json`. Refer to: [Getting Started](./docs/USAGE.md)

For the recommended hybrid setup, use a remote main model such as DeepSeek `deepseek-v4-pro` and a local OpenAI-compatible server such as vLLM serving Qwen for subagents. See [Hybrid Remote Main + Local Subagents](./docs/HYBRID_MODELS.md).

### Config Communication Channels

Before starting the backend, you should configure your preferred communication channels (Slack, Telegram, etc.) to enable message ingress and delivery. 📬

Use the interactive configuration tool:

```bash
xbot config --channel
```

List, configure, and log in to channels:

```bash
xbot channels list          # List all available channels
xbot channels status        # Show enabled/disabled state
xbot channels setup discord # Setup instructions (how to get tokens)
xbot channels login weixin  # Interactive login (QR code scan)
```

Use `channels setup <name>` to see step-by-step instructions for obtaining the required tokens and keys for any channel. For channels that support interactive login (Weixin QR code, WhatsApp bridge), use `channels login`. For manual configuration or detailed channel options, see [Getting Started](./docs/USAGE.md#5-channel-configuration).

> [!TIP]
> **Slack Users:** Set up Slack App for Agents [Slack Manual](https://www.meta-intelligence.tech/en/insight-openclaw-slack).
> **Telegram Users:** Set up Telegram App for Agents [Telegram Manual](https://www.meta-intelligence.tech/en/insight-openclaw-telegram).


## 🧾Usage

## CLI usage

xbot working on current folder by default on CLI mode, add `--workspace` parameter to assign working directory for xbot.

### One-shot prompt:

```bash
# this will scan and init the project for following tasks (XBOT.md)
xbot chat /init
# xbot chat /init --workspace ANOTHER_PROJECT_PATH
# this will do one task a time
xbot chat "find bugs in this project"
```

### Interactive shell (TUI, rich terminal UI):

```bash
xbot repl
# xbot repl --workspace ANOTHER_PROJECT_PATH
```

The CLI includes:
- 📡 Streamed responses
- 📜 Persistent history
- 💻 Local shell commands such as `/help` and `/clear`
- 🤖 Agent commands such as `/new`, `/clear`, `/memorize <text>`, `/status`, `/init` and `/stop`

`chat` and `repl` use the current directory as the workspace by default and create `.xbot/` there. Use `xbot repl --global` or `xbot chat --global "..."` for the configured global workspace, or `--workspace <path>` for an explicit workspace.

### Manage skills:

```bash
xbot skills list
xbot skills init my-custom-skill
```

## ⚡ Backend Bot
### Start the backend (Personal AI Assistant):

```bash
xbot run
```

`run` uses the configured global workspace by default. Use `xbot run --workspace .` when the backend should run against the current project workspace.

Sending task(s) to `xbot` using configured channels (such as Slack APP).

### Check runtime configuration and local state:

```bash
xbot status
xbot sessions
xbot jobs
xbot channels status
xbot skills list
```

## 📡 Runtime Surfaces

### Channel Backends

- 📧 **email**: IMAP polling + SMTP send
- 💬 **slack**: Socket Mode or webhook ingress + send
- ✈️ **telegram**: webhook ingress + send
- 🦘 **feishu**: webhook ingress + send, including inbound media/resource handling
- 🔔 **dingtalk**: Stream gateway WebSocket + REST send
- 🎮 **discord**: Gateway v10 WebSocket + REST send
- 🏠 **matrix**: CS API v3 long-poll sync + send
- 📱 **whatsapp**: WebSocket bridge to Node.js Baileys
- 🐧 **qq**: QQ Bot API WebSocket gateway + REST send
- 🏢 **wecom**: Enterprise WeChat AI Bot WebSocket
- 💬 **weixin**: Personal WeChat via HTTP long-poll
- 🌐 **mochat**: HTTP polling with session/panel support
- 🔌 **mcp**: stdio-based external tool servers exposed as native tools

### Channel Commands

When messaging the bot through Slack, Telegram, or other channels, you can send these signals as standalone messages:

- `stop` or `/stop` - Immediately stop the current agent task and cancel running subagents.
- `clear`, `new`, `/clear`, or `/new` - Start a new session and restore `.xbot/memory/HISTORY.md` to the default template.
- `memorize <text>` or `/memorize <text>` - Store durable user-directed memory in `.xbot/memory/MEMORY.md` through the `memory-entry-writer` summarization skill.
- `status` or `/status` - Get the current version and runtime usage stats.
- `help` or `/help` - Show available commands.

### Gateway Endpoints

The gateway exposes:

- ✅ `GET /healthz` - Health check
- 🟢 `GET /readyz` - Readiness check
- 📊 `GET /status` - Runtime status
- 📈 `GET /metrics` - Prometheus metrics
- 🎛️ `GET /admin` - Web admin UI
- 🔧 `GET /api/admin/*` - Admin API

## ✅ Verification

```bash
cargo fmt
cargo test
```

## 🎯 Use Cases

- 🤖 **Personal AI Assistant** - Always-on AI assistant across your communication channels
- 📊 **Automated Monitoring** - Scheduled tasks and webhook-based monitoring
- 🔧 **DevOps Automation** - Tool execution, file operations, and system management
- 📝 **Research & Reporting** - Web search, analysis, and report generation
- 🔄 **CI/CD Integration** - GitHub/CI automation and status updates

---

**Built with ❤️ in Rust** 🦀
