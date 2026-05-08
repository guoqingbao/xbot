# Hybrid Remote Main + Local Subagents

`rbot` can route the main task and background subagents to different model backends.

A common high-value setup is:

- main agent: remote API model such as DeepSeek `deepseek-v4-pro`
- subagents: local OpenAI-compatible server such as vLLM/Ollama/LM Studio serving Qwen

This gives the main turn a stronger synthesis model while letting subagents spend local GPU capacity on parallel repository search, file reads, tests, and bounded investigations.

## When To Use This

Use hybrid routing when:

- the main answer needs a stronger remote model
- subagents mostly do bounded exploration or implementation slices
- you want lower remote API spend during parallel work
- you have a local GPU box or LAN inference server available

In the TUI, the header and subagent cards show the active main model and subagent model, matching the screenshot in `docs/rbot.png`.

## Prerequisites

You need:

- a configured remote provider API key, for example DeepSeek
- a local or LAN OpenAI-compatible endpoint for the subagent model
- the model name exactly as the local endpoint expects it

Example local endpoint:

```text
http://127.0.0.1:8000/v1
```

Example LAN endpoint:

```text
http://10.0.0.50:9000/v1
```

## Start A Local Qwen Server

One option is vLLM:

```bash
python -m vllm.entrypoints.openai.api_server \
  --host 0.0.0.0 \
  --port 8000 \
  --model Qwen/Qwen3.6-27B-FP8
```

Use the model identifier that your server exposes. If your server exposes `Qwen3.6-27B-FP8`, put that exact value in `agents.subagents.model`.

## Configure Interactively

Run:

```bash
cargo run --release -- config --provider
```

Use the prompts to:

1. Configure the main provider as `deepseek`.
2. Input your deepseek api key and select the main model to `deepseek-v4-pro`.
3. Configure subagents with a separate `custom` provider.
4. Point subagents at the local OpenAI-compatible `apiBase`.

## Configure Manually

Edit `~/.rbot/config.json`.

Minimal example:

```json
{
  "agents": {
    "defaults": {
      "model": "deepseek-v4-pro",
      "provider": "deepseek",
      "contextWindowTokens": 262144,
      "maxTokens": 8192,
      "maxConcurrentTools": 5
    },
    "subagents": {
      "model": "Qwen3.6-27B-FP8",
      "provider": "local-qwen",
      "apiBase": "http://127.0.0.1:8000/v1"
    }
  },
  "providers": {
    "deepseek": {
      "apiBase": "https://api.deepseek.com",
      "apiKey": "sk-your-deepseek-key",
      "extraHeaders": {}
    },
    "local-qwen": {
      "apiBase": "http://127.0.0.1:8000/v1",
      "apiKey": "",
      "extraHeaders": {}
    }
  }
}
```

Notes:

- `agents.defaults` controls the main agent.
- `agents.subagents` controls spawned background subagents.
- `agents.subagents.provider` can be any provider key present under `providers`.
- `agents.subagents.apiBase` overrides the provider API base for subagents.
- Local or private-network API bases can use an empty `apiKey`.
- Do not commit real API keys or private endpoint details.

## Verify The Setup

Check the resolved configuration:

```bash
cargo run --release -- status
```

Start the TUI:

```bash
cargo run --release -- repl
```

Ask for a task that naturally uses delegation, for example:

```text
review the recent TUI changes and delegate independent checks to subagents
```

Expected behavior:

- the main header shows the remote main model, for example `deepseek-v4-pro`
- subagent cards show the local model, for example `Qwen3.6-27B-FP8`
- subagent tool work uses the local OpenAI-compatible endpoint

## Troubleshooting

If subagents fail to start:

- confirm the local server responds at `/v1/chat/completions`
- confirm `agents.subagents.model` matches the model name exposed by your server
- confirm `providers.<subagent-provider>.apiBase` or `agents.subagents.apiBase` includes `/v1`
- run `cargo run --release -- status` to inspect the active provider/model resolution

If the main agent uses the wrong model:

- set `agents.defaults.provider` explicitly to `deepseek`
- set `agents.defaults.model` explicitly to `deepseek-v4-pro`
- remove stale session model metadata by starting a new session with `/new`
