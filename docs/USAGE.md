# rbot Usage Guide

## 1. Install and Initialize

From the project root:

```bash
cd rbot
cargo run --release -- onboard
```

This creates:

- `~/.rbot/config.json`
- `~/.rbot/workspace/`
- a hidden runtime state directory at `<workspace>/.rbot/`
- workspace bootstrap files such as `.rbot/AGENTS.md`, `.rbot/SOUL.md`, `.rbot/USER.md`, `.rbot/TOOLS.md`, `.rbot/HEARTBEAT.md`, and memory files
- starter workspace skills under `.rbot/skills/`, including a memory-hygiene skill and editable project templates

## 2. Interactive Configuration

Instead of manually editing `~/.rbot/config.json`, you can use the interactive CLI:

### 2.1 Provider Configuration

Configure your LLM providers (OpenAI, Anthropic, OpenRouter, Ollama, vLLM, etc.):

```bash
cargo run --release -- config --provider
```

The CLI will guide you through:
1. Selecting a provider from the list.
2. Entering your API key (if required).
3. Fetching and selecting from available models.
4. Setting the default model and provider for the agent.
5. Optionally configuring a separate provider/API base or model for background subagents.

### 2.2 Channel Configuration

Configure communication channels (Telegram, Slack, Email, etc.):

```bash
cargo run --release -- config --channel
```

You can selectively enable channels, set permissions (`allowFrom`), and provide necessary tokens or secrets interactively.

## 3. Choose a Provider (Manual)

`rbot` talks to providers through an OpenAI-compatible chat interface.

Supported practical modes:

- Remote API providers such as OpenAI-compatible gateways
- Local engines such as Ollama and vLLM
- Custom local or remote OpenAI-compatible servers

### Openrouter example

```json
{
  "agents": {
    "defaults": {
      "model": "minimax/minimax-m2.7",
      "provider": "openrouter"
    }
  },
  "providers": {
    "openrouter": {
      "apiKey": "sk-or-v1-...",
      "extraHeaders": {}
    }
  }
}
```

### OpenAI-compatible remote example

```json
{
  "agents": {
    "defaults": {
      "model": "openai/gpt-4.1-mini",
      "provider": "openai"
    }
  },
  "providers": {
    "openai": {
      "apiKey": "sk-..."
    }
  }
}
```

### Ollama example

`rbot` supports Ollama as a local provider without requiring an API key.

Start Ollama first:

```bash
ollama serve
ollama pull qwen2.5-coder:7b
```

Then configure:

```json
{
  "agents": {
    "defaults": {
      "model": "ollama/qwen2.5-coder:7b",
      "provider": "ollama"
    }
  },
  "providers": {
    "ollama": {
      "apiBase": "http://localhost:11434/v1"
    }
  }
}
```

### vLLM example

If you are serving a model with vLLM on port `8000`:

```json
{
  "agents": {
    "defaults": {
      "model": "vllm/Qwen/Qwen2.5-7B-Instruct",
      "provider": "vllm"
    }
  },
  "providers": {
    "vllm": {
      "apiBase": "http://localhost:8000/v1"
    }
  }
}
```

### LM Studio or another local OpenAI-compatible server

Use the `custom` provider:

```json
{
  "agents": {
    "defaults": {
      "model": "custom/local-model",
      "provider": "custom"
    }
  },
  "providers": {
    "custom": {
      "apiBase": "http://127.0.0.1:1234/v1",
      "apiKey": ""
    }
  }
}
```

Notes:

- Known local providers such as `ollama` and `vllm` do not require an API key.
- Custom providers can use an empty API key when the upstream server does not require auth.
- The model string can be any identifier accepted by the target backend.

### Subagent model/provider override

By default, background subagents use the same provider and model as the main task. You can run subagents on a cheaper or faster model by setting `agents.subagents`.

Example: main task uses a heavier OpenAI model, while subagents use a local OpenAI-compatible server:

```json
{
  "agents": {
    "defaults": {
      "model": "openai/gpt-4.1",
      "provider": "openai"
    },
    "subagents": {
      "model": "qwen2.5-coder:7b",
      "provider": "subagent-fast",
      "apiBase": "http://127.0.0.1:8001/v1"
    }
  },
  "providers": {
    "openai": {
      "apiKey": "sk-..."
    },
    "subagent-fast": {
      "apiKey": "",
      "apiBase": "http://127.0.0.1:8001/v1"
    }
  }
}
```

`agents.subagents.model` is optional. If it is empty or omitted, subagents inherit the main task model. `agents.subagents.provider` defaults to `"auto"`. For an OpenAI-compatible local or remote backend, use a provider key such as `subagent-fast` and set `apiBase` either under `agents.subagents` or in the matching `providers` entry.

## 3. Run Modes

### One-shot prompt

```bash
cargo run --release -- chat "summarize the codebase"
```

### Interactive shell

```bash
cargo run --release -- repl
```

The interactive shell is designed for day-to-day agent work:

- persistent command history in `~/.rbot/history.txt`
- streamed model output instead of waiting for the full reply
- queued prompts while a turn is already running; queued turns start automatically when the current turn ends
- local shell commands: `/help`, `/clear`, `/exit`
- agent commands forwarded to the runtime: `/new`, `/status`, `/stop`
- multiline input by ending a line with `\`
- the welcome header shows both the current working directory and the configured workspace
- the header also shows the active hidden state root under `<workspace>/.rbot`
- tool activity is shown with emoji-based pills such as file, shell, web, message, and cron actions
- fenced code blocks in replies are syntax-highlighted in the CLI by language when ANSI colors are available
- CLI session history is scoped by current working directory, so different projects do not share the same chat thread

For project-local development, set:

```json
{
  "agents": {
    "defaults": {
      "workspace": "."
    }
  }
}
```

## 3.1 Workspace Memory

`rbot` uses two memory files inside `workspace/.rbot/`:

- `.rbot/memory/MEMORY.md`: permanent memory store, capped at `agents.defaults.memoryMaxBytes` bytes
- `.rbot/memory/HISTORY.md`: resettable history log for later search and consolidation

Operational rule:

- completed user tasks are summarized into `MEMORY.md` with title, summary, attention points, and finish time through the `memory-entry-writer` skill
- explicit `memorize` or `/memorize <text>` requests are summarized through the same skill and stored in `MEMORY.md` as user instructed memory
- new tasks load only topic-relevant slices from `MEMORY.md`, not the entire file
- `clear` / `/clear` / `new` / `/new` resets the current session and restores `HISTORY.md` to the default template

New workspaces now include starter guidance, an always-on memory skill, and a dedicated `memory-entry-writer` skill so memory writes stay compact instead of copying large reply fragments.

Example config:

```json
{
  "agents": {
    "defaults": {
      "memoryMaxBytes": 32768
    }
  }
}
```

### Long-running backend

```bash
cargo run -- run
```

`run` starts:

- the provider client
- the agent runtime
- cron jobs
- heartbeat review
- enabled channels
- the HTTP gateway
- the admin API and UI
- the metrics endpoint

### Slack without a public webhook

Slack supports two practical modes in `rbot`:

- `webhook`: Slack sends Events API requests to your public HTTPS endpoint
- `socket`: `rbot` opens an outbound WebSocket to Slack and does not require a public webhook URL (Public)

Example Socket Mode config:

```json
{
  "channels": {
    "slack": {
      "enabled": true,
      "mode": "socket",
      "allowFrom": ["*"],
      "botToken": "xoxb-...",
      "appToken": "xapp-...",
      "replyInThread": true,
      "groupPolicy": "mention"
    }
  }
}
```

Notes:

- `mode: "socket"` requires both `botToken` and `appToken`
- you do not need `signingSecret` or a public `/slack/events` URL in socket mode
- in webhook mode, you still need a public HTTPS URL configured in Slack Event Subscriptions

## 4. Gateway Endpoints

When `run` is active, the gateway exposes:

- `GET /healthz`
- `GET /readyz`
- `GET /status`
- `GET /metrics`
- `GET /admin`
- `GET /api/admin/overview`
- `GET /api/admin/sessions`
- `GET /api/admin/cron`

The bind address comes from:

```json
{
  "gateway": {
    "host": "0.0.0.0",
    "port": 18790
  }
}
```

Admin and metrics paths can also be customized:

```json
{
  "gateway": {
    "admin": {
      "enabled": true,
      "path": "/admin"
    },
    "metrics": {
      "enabled": true,
      "path": "/metrics"
    }
  }
}
```

## 5. Channel Configuration

### Email

Email is polling-driven and does not require webhooks.

```json
{
  "channels": {
    "email": {
      "enabled": true,
      "allowFrom": ["*"],
      "consentGranted": true,
      "imapHost": "imap.example.com",
      "imapPort": 993,
      "imapUsername": "bot@example.com",
      "imapPassword": "...",
      "imapMailbox": "INBOX",
      "imapUseSsl": true,
      "smtpHost": "smtp.example.com",
      "smtpPort": 587,
      "smtpUsername": "bot@example.com",
      "smtpPassword": "...",
      "smtpUseTls": true,
      "fromAddress": "bot@example.com",
      "autoReplyEnabled": true,
      "pollIntervalSeconds": 30
    }
  }
}
```

### Slack

Slack is currently webhook-driven in `rbot`.

```json
{
  "channels": {
    "sendProgress": true,
    "sendToolHints": false,
    "slack": {
      "enabled": true,
      "allowFrom": ["*"],
      "botToken": "xoxb-...",
      "signingSecret": "...",
      "webhookPath": "/slack/events",
      "replyInThread": true,
      "groupPolicy": "mention"
    }
  }
}
```

Operational notes:

- `signingSecret` is required for startup validation.
- Point Slack event subscriptions at `http://<host>:<port>/slack/events`.
- Send software-development tasks as normal messages or mentions, for example: `review this repo, run tests, and fix failures`.
- `channels.sendToolHints` defaults to `false`; in that mode, `rbot` sends a muted-tool notice on the first tool call and batch summaries every 10 tool calls or before the next non-tool reply.
- Set `channels.sendToolHints` to `true` if you want every tool execution hint sent back to Slack while a task is running.

### Telegram

Telegram is currently webhook-driven in `rbot`.

```json
{
  "channels": {
    "sendProgress": true,
    "sendToolHints": false,
    "telegram": {
      "enabled": true,
      "allowFrom": ["*"],
      "token": "<bot-token>",
      "webhookPath": "/telegram/webhook",
      "webhookSecret": "optional-shared-secret",
      "replyToMessage": true,
      "groupPolicy": "mention"
    }
  }
}
```

Set the Telegram webhook externally to:

`https://<your-domain>/telegram/webhook`

If `webhookSecret` is configured, Telegram requests must include the matching secret header.

Usage notes:

- Send development or analysis tasks as plain messages to the bot.
- In groups, `groupPolicy: "mention"` keeps the bot from reacting to every message.
- `channels.sendToolHints` defaults to `false`; in that mode, `rbot` sends a muted-tool notice on the first tool call and batch summaries every 10 tool calls or before the next non-tool reply.
- Set `channels.sendToolHints` to `true` if you want every tool execution hint sent back to Telegram while a task is running.

### Feishu

Feishu runs through the webhook gateway and supports inbound text, post, interactive cards, replies, and media/resource download.

```json
{
  "channels": {
    "sendProgress": true,
    "sendToolHints": false,
    "feishu": {
      "enabled": true,
      "allowFrom": ["*"],
      "appId": "cli_xxx",
      "appSecret": "...",
      "verificationToken": "...",
      "webhookPath": "/feishu/events",
      "groupPolicy": "mention",
      "replyToMessage": true,
      "reactEmoji": "THUMBSUP"
    }
  }
}
```

Point Feishu event subscriptions at:

`https://<your-domain>/feishu/events`

Usage notes:

- Mention the bot in group chats when using `groupPolicy: "mention"`.
- Development tasks can be sent as normal text instructions, and Feishu replies can include dedicated tool-hint cards during execution.
- `channels.sendToolHints` defaults to `false`; in that mode, `rbot` sends a muted-tool notice on the first tool call and batch summaries every 10 tool calls or before the next non-tool reply.
- Set `channels.sendToolHints` to `true` if you want every tool execution hint card sent back during execution. Non-tool progress messages are still controlled by `channels.sendProgress`.

## 6. Combined Example

```json
{
  "agents": {
    "defaults": {
      "workspace": "~/.rbot/workspace",
      "model": "ollama/qwen2.5-coder:7b",
      "provider": "ollama",
      "maxToolIterations": 0,
      "contextWindowTokens": 65536
    },
    "subagents": {
      "model": "",
      "provider": "auto"
    }
  },
  "providers": {
    "ollama": {
      "apiBase": "http://localhost:11434/v1"
    }
  },
  "gateway": {
    "host": "0.0.0.0",
    "port": 18790,
    "heartbeat": {
      "enabled": true,
      "intervalS": 1800
    }
  },
  "channels": {
    "telegram": {
      "enabled": true,
      "allowFrom": ["*"],
      "token": "<bot-token>",
      "webhookPath": "/telegram/webhook"
    }
  }
}
```

`maxToolIterations: 0` means the agent loop is unbounded. Use a positive number only when you want a hard ceiling on tool calls.

## 7. MCP Tool Servers

`rbot` supports MCP over `stdio`. Enabled MCP tools are registered as native tools using names like `mcp_<server>_<tool>`.

Example:

```json
{
  "tools": {
    "mcpServers": {
      "github": {
        "enabled": true,
        "type": "stdio",
        "command": "npx",
        "args": ["-y", "@modelcontextprotocol/server-github"],
        "enabledTools": ["*"],
        "toolTimeout": 30
      }
    }
  }
}
```

Current scope:

- `stdio` transport is supported
- startup validation fails fast if an enabled MCP server has no command
- unsupported transports are rejected during startup

## 8. Built-in Skills

Built-in skills ship with the repository under `rbot/skills/`.

Current built-in set:

- `memory-hygiene` (always-on)
- `memory` (always-on)
- `memory-entry-writer`
- `workspace-operator`
- `software-engineer`
- `data-analyst`
- `github-cli`
- `github`
- `scheduled-ops`
- `cron`
- `clawhub`
- `skill-creator`
- `summarize`
- `weather`
- `tmux`

Behavior:

- always-on skills are injected automatically
- relevant task-specific skills are suggested and loaded based on prompt keywords
- skills with unmet requirements (missing binaries, env vars, or OS) are marked unavailable
- workspace-local skills live under `<workspace>/.rbot/skills/<name>/SKILL.md`
- new workspaces also get starter workspace skill templates that you can edit for project-specific context and delivery rules

### Skill Management

List all skills and their availability:

```bash
cargo run -- skills list
```

Scaffold a new skill:

```bash
cargo run -- skills init my-custom-skill
```

This creates `<workspace>/.rbot/skills/my-custom-skill/SKILL.md` with a starter template.

## 9. Useful Commands

Print the resolved config:

```bash
cargo run -- print-config
```

Run a different model without changing config:

```bash
cargo run -- run --model ollama/qwen2.5-coder:7b
```

Start a one-shot request against a specific model:

```bash
cargo run -- chat --model ollama/qwen2.5-coder:7b "list the next implementation tasks"
```

Inspect runtime state without starting the daemon:

```bash
cargo run -- sessions            # List active chat sessions
cargo run -- jobs                # List scheduled cron jobs
cargo run -- print-config        # Print current resolved config
cargo run -- config --provider   # Interactive provider setup
cargo run -- config --channel    # Interactive channel setup
```

### Channel Management

```bash
cargo run -- channels list       # List all available channels
cargo run -- channels status     # Show enabled/disabled state per channel
cargo run -- channels login      # Interactive login (e.g. Weixin QR code scan)
cargo run -- channels login weixin   # Login to a specific channel
cargo run -- channels setup      # Show setup instructions for a channel
cargo run -- channels setup discord  # Setup instructions for a specific channel
```

### Skill Management

```bash
cargo run -- skills list         # List skills with availability status
cargo run -- skills init NAME    # Scaffold a new skill directory
```

## 10. Operational Notes

- `run` validates enabled channel config before startup.
- `run` also validates enabled MCP server configuration before startup.
- Local providers are accepted without API keys when the provider is recognized as local.
- Outbound runtime/system errors are surfaced through the runtime logs instead of being silently dropped.
- Feishu media downloads are stored under `~/.rbot/media/feishu`.
- The admin UI polls the runtime every few seconds and exposes channel controls plus heartbeat triggering.
- The metrics endpoint exposes Prometheus-compatible counters and gauges for message counts, provider requests, token totals, latency, and throughput.

## 11. Additional Channel Configuration

Each channel section below includes how to obtain the required credentials and the config format. You can also run `rbot channels setup <name>` to see setup instructions in the terminal.

### DingTalk

DingTalk uses the Stream gateway WebSocket protocol.

**How to obtain credentials:**

1. Go to <https://open-dev.dingtalk.com> and create a robot application
2. Under **Credentials**, copy the Client ID (AppKey) and Client Secret (AppSecret)
3. Under **Robot**, enable the robot and copy the Robot Code

```json
{
  "channels": {
    "dingtalk": {
      "enabled": true,
      "allowFrom": ["*"],
      "clientId": "<AppKey from developer console>",
      "clientSecret": "<AppSecret from developer console>",
      "robotCode": "<Robot Code>"
    }
  }
}
```

### Discord

Discord connects via the Gateway v10 WebSocket.

**How to obtain credentials:**

1. Go to <https://discord.com/developers/applications> and create an application
2. Under **Bot**, click "Add Bot" and copy the bot token
3. Under **Bot**, enable "Message Content Intent" in Privileged Gateway Intents
4. Under **OAuth2 > URL Generator**, select `bot` scope with `Send Messages` permission
5. Use the generated URL to invite the bot to your server

```json
{
  "channels": {
    "discord": {
      "enabled": true,
      "allowFrom": ["*"],
      "botToken": "<your-bot-token>",
      "groupPolicy": "mention"
    }
  }
}
```

`groupPolicy` options: `"mention"` (respond only when @mentioned), `"open"` (respond to all messages).

### Matrix

Matrix uses the CS API v3 long-poll `/sync` endpoint.

**How to obtain credentials:**

1. Create a bot account on your Matrix homeserver
2. Obtain an access token (e.g. via Element: Settings > Help & About > Access Token)
3. Note the full user ID (e.g. `@bot:example.com`)
4. Invite the bot to the rooms where it should respond

```json
{
  "channels": {
    "matrix": {
      "enabled": true,
      "allowFrom": ["*"],
      "homeserverUrl": "https://matrix.example.com",
      "accessToken": "<your-access-token>",
      "userId": "@bot:example.com"
    }
  }
}
```

Note: End-to-end encrypted rooms (`m.room.encrypted`) are not supported; the bot will skip encrypted messages.

### WhatsApp

WhatsApp connects to a Node.js Baileys bridge via WebSocket.

**Setup steps:**

1. Install Node.js v18+
2. Clone the Baileys bridge: `git clone https://github.com/nicepkg/whatsapp-bridge`
3. `cd whatsapp-bridge && npm install && npm start`
4. Scan the QR code displayed in the bridge terminal with WhatsApp
5. The bridge saves auth state — subsequent starts reconnect automatically

Run `rbot channels login whatsapp` for step-by-step guidance.

```json
{
  "channels": {
    "whatsapp": {
      "enabled": true,
      "allowFrom": ["*"],
      "bridgeUrl": "ws://localhost:3001",
      "bridgeToken": "",
      "groupPolicy": "open"
    }
  }
}
```

The bridge must be running before `rbot run`. Set `bridgeToken` if your bridge instance requires authentication.

### QQ

QQ uses the QQ Bot API with WebSocket gateway.

**How to obtain credentials:**

1. Go to <https://q.qq.com> and register as a QQ Bot developer
2. Create a bot application and obtain the App ID and Secret
3. Configure the bot's intents and permissions in the developer console

```json
{
  "channels": {
    "qq": {
      "enabled": true,
      "allowFrom": ["*"],
      "appId": "<your-app-id>",
      "secret": "<your-secret>"
    }
  }
}
```

### WeCom

WeCom (Enterprise WeChat) uses the AI Bot WebSocket protocol.

**How to obtain credentials:**

1. Log in to <https://work.weixin.qq.com> admin console
2. Create a self-built application under **App Management**
3. Note the Corp ID (from **My Enterprise**), Agent ID, and Secret

```json
{
  "channels": {
    "wecom": {
      "enabled": true,
      "allowFrom": ["*"],
      "corpId": "<your-corp-id>",
      "agentId": "<your-agent-id>",
      "secret": "<your-secret>"
    }
  }
}
```

All three fields (`corpId`, `agentId`, `secret`) are required — `agentId`/`secret` for WebSocket auth, `corpId` for outbound message delivery.

### Weixin

Weixin (personal WeChat) uses HTTP long-poll with QR code login via the ilinkai API.

**Login flow:**

1. Enable the channel in config (no token needed initially)
2. Run `rbot channels login weixin` — a QR code URL will be printed
3. Open the URL in WeChat and scan to authorize
4. The token is saved to `<stateDir>/account.json` for future sessions

Alternatively, run `rbot run` and the QR login starts automatically if no saved token is found.

```json
{
  "channels": {
    "weixin": {
      "enabled": true,
      "allowFrom": ["*"]
    }
  }
}
```

### Mochat

Mochat connects to a Mochat/OpenClaw instance via HTTP polling.

**How to obtain credentials:**

1. Obtain a Claw Token from your Mochat or OpenClaw instance admin
2. Note the session IDs and/or panel IDs you want the bot to monitor

```json
{
  "channels": {
    "mochat": {
      "enabled": true,
      "allowFrom": ["*"],
      "baseUrl": "https://your-instance.com",
      "clawToken": "<your-token>",
      "sessions": ["session-id-1"],
      "panels": []
    }
  }
}
```

## 12. Additional Provider Configuration

### Anthropic

Anthropic is supported natively with the Messages API:

```json
{
  "agents": {
    "defaults": {
      "model": "anthropic/claude-sonnet-4-20250514",
      "provider": "anthropic"
    }
  },
  "providers": {
    "anthropic": {
      "apiKey": "sk-ant-..."
    }
  }
}
```

Optional `reasoningEffort` can be set to control extended thinking behavior.

### GitHub Copilot

GitHub Copilot is an OAuth provider and does not require an API key:

```json
{
  "agents": {
    "defaults": {
      "model": "github_copilot/gpt-4o",
      "provider": "github_copilot"
    }
  },
  "providers": {
    "github_copilot": {}
  }
}
```

### Cursor

Cursor requires an explicit `apiBase`:

```json
{
  "agents": {
    "defaults": {
      "model": "cursor/gpt-4o",
      "provider": "cursor"
    }
  },
  "providers": {
    "cursor": {
      "apiKey": "your-key",
      "apiBase": "https://your-cursor-api-base"
    }
  }
}
```

## 13. Concurrency Configuration

Control how many inbound messages are processed concurrently:

```json
{
  "agents": {
    "defaults": {
      "maxConcurrentRequests": 3
    }
  }
}
```

Messages for the same session are always serialized regardless of this setting.

## 14. Channel Delivery Configuration

Configure outbound delivery behavior:

```json
{
  "channels": {
    "sendProgress": true,
    "sendToolHints": false,
    "sendMaxRetries": 3
  }
}
```

- `sendMaxRetries`: number of delivery attempts with exponential backoff (1s, 2s, 4s...) before giving up.

## 15. Current Scope

The supported production channel set in this repository is:

- `email`
- `slack`
- `telegram`
- `feishu`
- `dingtalk`
- `discord`
- `matrix`
- `whatsapp`
- `qq`
- `wecom`
- `weixin`
- `mochat`

The runtime is designed so additional providers and transports can be added behind the same trait boundaries without changing the agent loop.
