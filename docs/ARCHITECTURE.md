# xbot Architecture

## Runtime Model

`xbot` is organized as a message-driven runtime:

1. A channel receives inbound user activity.
2. The channel publishes an `InboundMessage` onto the bus.
3. `AgentRuntime` acquires a global semaphore permit and a per-session lock, then invokes `AgentLoop`.
4. `AgentLoop` builds context, calls the model, executes tools, and persists the turn.
5. Outbound messages are published back onto the bus.
6. `ChannelManager` delivers outbound messages through the target transport, with retry logic and stream delta support.

This keeps transport, orchestration, and model execution separate. The global semaphore (`max_concurrent_requests`, default 3) prevents unbounded parallel processing, and the per-session mutex ensures messages for the same session are serialized.

## Main Components

| Module | Responsibility |
| --- | --- |
| `src/engine/orchestrator.rs` | Agent turn loop, session commands, tool iteration |
| `src/engine/context.rs` | Runtime context assembly from workspace files, media, and topic-relevant memory |
| `src/engine/hook.rs` | `AgentHook` trait for lifecycle callbacks (streaming, iteration, tool execution) |
| `src/storage/session_store.rs` | JSONL-backed session storage |
| `src/engine/memory.rs` | Permanent memory storage, LLM-driven consolidation, history reset, relevance filtering |
| `src/tools.rs` | Tool registry and built-in tool implementations |
| `src/runtime/worker.rs` | Bus worker with global semaphore and per-session serialization |
| `src/channels/` | Transport adapters, channel manager, stream delta coalescing, and retry logic |
| `src/runtime/http.rs` | HTTP ingress plus health/readiness/status endpoints |
| `src/observability.rs` | Runtime telemetry, metrics, provider instrumentation, and system/provider snapshots |
| `src/cron.rs` | Scheduled jobs and execution history |
| `src/runtime/heartbeat.rs` | Periodic task review loop |
| `src/providers/` | Provider clients, Anthropic native support, and registry metadata |
| `src/runtime/bootstrap.rs` | Backend startup validation, OAuth handling, and provider construction |
| `src/integrations/mcp.rs` | MCP stdio client and tool registration |
| `src/cli/channels_cli.rs` | `xbot channels list/status/login` CLI subcommands |
| `src/cli/skills_cli.rs` | `xbot skills list/init` CLI subcommands |

## Design Choices

### Domain-first module layout

The crate is organized by runtime domain instead of by a flat file list:

- `engine/` owns reasoning, context construction, memory policy, skills, hook lifecycle, and background subtasks
- `runtime/` owns process wiring, HTTP ingress, validation, concurrency control, and long-running services
- `storage/` owns session persistence and the internal message bus
- `channels/` owns transport adapters, stream delta coalescing, and retry logic
- `providers/` owns model backends, Anthropic native support, and provider metadata
- `integrations/` owns external protocol bridges such as MCP
- `observability.rs` owns metrics and monitoring state shared across the runtime
- `cli/` owns interactive configuration, channel management, and skill management subcommands

That layout keeps operational concerns separate from agent behavior and avoids coupling transports, storage, and orchestration together.

### Trait-based boundaries

Providers, tools, and channels are all expressed as traits. That keeps the runtime swappable and makes transport-specific logic independent from agent execution.

### Message bus orchestration

The bus is the seam between transport and reasoning. Channels do not call the agent directly, and the agent does not know how messages are physically delivered.

### Persistent workspace state

Sessions, memory files, cron jobs, and skills are stored on disk so the runtime can be restarted without losing state.

`MEMORY.md` is the permanent store for durable facts and structured task summaries, while `HISTORY.md` is a resettable event log. The context builder reads only memory slices relevant to the current task instead of injecting the whole file. Completed tasks and `/memorize` requests are condensed into structured memory entries through the `memory-entry-writer` skill before they are appended.

### LLM-driven memory consolidation

When the session exceeds 75% of the context window, the consolidator attempts LLM-driven summarization first: it sends a chunk of older messages to the provider and parses a structured JSON response containing a history entry and an optional durable memory update. If the LLM call fails 3 consecutive times, the consolidator falls back to raw archive (appending message text directly to `HISTORY.md`). This matches nanobot's consolidation strategy while keeping the implementation idiomatic in Rust.

### Hook-based extensibility

The `AgentHook` trait provides lifecycle callbacks (`before_iteration`, `on_stream`, `on_stream_end`, `before_execute_tools`, `after_iteration`, `finalize_content`) that allow extending the agent loop without modifying its core logic. The `CallbackHook` implementation bridges streaming tokens and progress updates to the existing message bus.

### OpenAI-compatible provider contract

`xbot` uses an OpenAI-compatible chat-completions contract as the common provider interface. That keeps remote APIs and local runtimes behind the same operational path.

## Backend Operation

`xbot run` starts:

- provider client
- agent runtime
- cron service
- heartbeat service
- enabled channels
- HTTP gateway

The same process also exposes:

- the admin UI
- the admin JSON API
- the Prometheus metrics endpoint

The HTTP gateway is operationally useful even when the main traffic is not HTTP because it exposes:

- `GET /healthz`
- `GET /readyz`
- `GET /status`

## Channel Model

The currently supported transport set is:

- `email` - IMAP polling + SMTP send
- `slack` - Socket Mode WebSocket or webhook + REST send
- `telegram` - webhook + REST send
- `feishu` - webhook + REST send with media handling
- `dingtalk` - Stream gateway WebSocket + REST batch send
- `discord` - Gateway v10 WebSocket + REST send with mention/typing support
- `matrix` - CS API v3 long-poll `/sync` + REST send
- `whatsapp` - WebSocket bridge to Node.js Baileys
- `qq` - QQ Bot API WebSocket gateway + REST send
- `wecom` - Enterprise WeChat AI Bot WebSocket
- `weixin` - Personal WeChat via HTTP long-poll (QR login)
- `mochat` - HTTP polling with session/panel support

Each channel owns:

- its transport-specific config
- inbound normalization
- outbound formatting and delivery
- any channel-specific file/media handling
- optional `send_delta` for streaming-capable transports

The `ChannelManager` dispatch loop handles:

- stream delta coalescing (`_stream_delta` / `_stream_end` metadata)
- configurable retry with exponential backoff (`send_max_retries`, default 3)
- muted tool hint batching and summary generation

## Local Provider Support

Local providers are treated as normal backends when they expose an OpenAI-style API. The runtime does not require an API key for known local engines such as:

- `ollama`
- `vllm`

Custom local gateways can also be configured through the `custom` provider entry.

## MCP Integration

Enabled MCP servers are connected during agent startup. Their tool definitions are registered into the same tool registry as the built-in filesystem, shell, web, cron, and messaging tools.

Current transport support:

- `stdio`

The startup path validates MCP configuration before the runtime begins serving traffic.

## Testing Strategy

`xbot` uses both module-local unit tests and integration tests:

- unit tests live next to the code for parsing, config validation, and loader behavior
- integration tests under `tests/` exercise runtime flow, channel behavior, and backend wiring

The split keeps low-level behavior close to its implementation while preserving end-to-end coverage for the public runtime surfaces.
