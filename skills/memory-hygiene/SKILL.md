---
name: memory-hygiene
description: "Always-on guidance for maintaining durable workspace memory and searching historical context."
metadata: {"xbot":{"always":true,"triggers":["memory","cleanup","consolidate","prune"]}}
---

# Memory Hygiene

Use this guidance in every workspace.

## Two-Layer Memory

- `memory/MEMORY.md` is active long-term context. Keep it concise and curated.
- `memory/HISTORY.md` is an append-only log for later search. It is not active context.

## Update MEMORY.md When

- the user states a stable preference
- you discover a durable project fact or architecture rule
- a decision will matter in later sessions
- a recurring process or operational constraint becomes clear

## Search HISTORY.md When

- you need to recall prior events, experiments, or earlier conclusions
- you suspect a detail was discussed before but is not durable enough for `MEMORY.md`
- you want to recover context after a session reset

## Practical Rules

1. Keep `MEMORY.md` short and structured.
2. Do not dump full transcripts or raw logs into long-term memory.
3. Promote durable facts from `HISTORY.md` into `MEMORY.md` when they become important.
4. Prefer updating memory immediately after discovering durable context rather than waiting until the end.
