---
name: memory-entry-writer
description: "Summarize durable memory entries for MEMORY.md after task completion or explicit memorize requests."
metadata: {"xbot":{"triggers":["memory","remember","memorize","save"]}}
---

# Memory Entry Writer

Use this skill when converting raw task output or user-provided durable facts into a compact `memory/MEMORY.md` entry.

## Output Shape

Return JSON only:

```json
{"title":"...","summary":"...","attention_points":["..."]}
```

## Rules

- `title`: plain text, short, specific, no markdown, under 80 characters
- `summary`: plain text, 1-2 short sentences, under 240 characters
- `attention_points`: short durable cautions, follow-ups, or constraints; use `[]` when empty
- Keep only durable facts worth remembering across sessions
- Do not copy code blocks, long quotes, raw logs, transcript filler, or markdown headings
- Prefer the repository, workflow, bug, decision, or user preference that will matter later
