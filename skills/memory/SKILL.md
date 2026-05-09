---
name: memory
description: "Two-layer workspace memory, automatic session consolidation, and how to search the history log."
always: true
metadata: {"xbot":{"description":"Long-term MEMORY.md vs append-only HISTORY.md; when to write; consolidation; grep recall.","always":true,"emoji":"🧠","triggers":["memory","history","recall","consolidate","remember"]}}
---

# Memory

xbot keeps project memory under the configured workspace, not at the repository root by itself. Paths are relative to that workspace:

- **`.xbot/memory/MEMORY.md`** — Curated long-term context (preferences, architecture, durable decisions). Loaded into the agent context (often via topic slices), size-limited by `agents.defaults.memoryMaxBytes`.
- **`.xbot/memory/HISTORY.md`** — Append-only log of consolidation and archived conversation chunks. **Not** treated as active context; search it when you need to recover past events or details.

## When to write to MEMORY.md

Update long-term memory when something should survive future sessions:

- Stable user preferences or standing requests
- Project facts: architecture, conventions, build/test commands, env constraints
- Decisions that will matter after a session reset or channel switch
- Durable summaries worth recalling (see the `memory-entry-writer` skill for structured entries)

Avoid pasting full transcripts, raw logs, or ephemeral debugging notes into `MEMORY.md`. Put recoverable-but-not-evergreen detail in `HISTORY.md` (via consolidation) or leave it in the chat.

## When to search HISTORY.md

Use `HISTORY.md` when:

- You need a prior experiment, conclusion, or dated event
- The user asks “what did we do before?” and it is not in `MEMORY.md`
- You are debugging memory drift and want to see what was consolidated

## Auto-consolidation

When the session grows large relative to the model context window, xbot **automatically** consolidates older turns:

1. **Preferred path:** The runtime asks the model to summarize a chunk of conversation. A summary is appended to `HISTORY.md`, and an optional short update can be appended to `MEMORY.md` as a consolidation entry.
2. **Fallback:** If consolidation fails repeatedly, raw message text is archived into `HISTORY.md` in chunks until usage drops back near a safe threshold (roughly three quarters of the configured context budget).

You do not need to trigger consolidation manually. Session commands like `clear` / `new` reset the chat thread; `HISTORY.md` may be reset depending on product behavior—check current docs if that matters for your workflow.

## Grep strategies for HISTORY.md

Pick a strategy based on file size:

- **Small files:** Read the file or a section, then filter mentally.
- **Large or old logs:** Use the shell from the workspace root so paths resolve.

Examples (macOS/Linux):

```bash
# Case-insensitive, show line numbers
grep -ni "keyword" .xbot/memory/HISTORY.md

# Last N matching lines
grep -i "deploy" .xbot/memory/HISTORY.md | tail -n 30

# Ripgrep: fast, respects .gitignore unless you pass --no-ignore
rg -n "pattern" .xbot/memory/HISTORY.md
```

Windows (cmd): `findstr /i "keyword" .xbot\memory\HISTORY.md`

Prefer **narrow patterns** (component names, ticket IDs, error strings) over single common words to avoid noise.

## Relation to other skills

- **`memory-hygiene`** — Day-to-day rules for keeping memory tidy.
- **`memory-entry-writer`** — Structured writes into `MEMORY.md` for tasks and explicit memorize requests.
