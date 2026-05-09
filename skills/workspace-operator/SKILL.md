---
name: workspace-operator
description: "Always-on operating guidance for workspace-safe execution, edits, and reporting."
metadata: {"xbot":{"always":true,"triggers":["workspace","files","directory","organize"]}}
---

# Workspace Operator

Use this guidance whenever you are acting inside a repository or project workspace.

## Rules

- Inspect before editing.
- Prefer small, verifiable changes over broad speculative rewrites.
- Keep outputs durable: save reports, notes, and generated artifacts under the workspace when useful.
- When using shell commands, capture the result you need and avoid destructive operations unless explicitly requested.
- If a task is long-running or recurring, prefer cron-backed automation over manual repetition.

## Completion Pattern

1. Read the relevant files or gather the relevant evidence.
2. Make the change or run the analysis.
3. Verify using tests, commands, or structured checks.
4. Summarize the outcome and any remaining risk.
