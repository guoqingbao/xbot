---
name: software-engineer
description: "Software engineering workflow for planning, code changes, tests, CI-style validation, subagent delegation, and release hygiene."
metadata: {"xbot":{"triggers":["code","coding","vibe coding","implement","debug","bug","fix","test","tests","refactor","programming","repo","codebase"]}}
---

# Software Engineer

Apply this workflow for autonomous software development tasks.

## Workflow

1. Establish scope from the request and current code state (find if there is a XBOT.md in the current workspace, use it if exists, otherwise ask user if we need to use /init command to create one).
2. Read the touched code paths before proposing or applying edits.
3. Prefer minimal coherent patches that preserve surrounding behavior.
4. Run focused verification first, then broader tests if needed.
5. If repository automation exists, inspect lint/build/test outputs and iterate on failures.

## Subagents

Use subagents when a task naturally splits into independent work streams, especially repository-wide investigation, per-directory review, independent bug hunts, parallel research, or broad test/failure triage. Keep the main agent responsible for coordination, final synthesis, and edits that require cross-cutting judgment.

Good delegation pattern:

1. Split work into concrete, bounded subtasks with clear ownership such as a folder, module, failing test group, or research question.
2. Spawn no more subagents than the concurrency limit allows. If more slices exist, batch them.
3. Give each subagent a self-contained instruction and expected output format.
4. After spawning subagents, call `wait_subagents` to receive their final text results before continuing the main task.
5. Synthesize subagent results in the main task context. Do not claim a subagent found or changed something unless its returned result supports it.

Use the main agent directly instead of subagents when the next step is tightly coupled, requires a single coherent edit across many files, or depends on immediate local context that would make delegation slower or less reliable.

## Engineering Standards

- Use the filesystem and shell tools to inspect, edit, and verify.
- Keep changes reviewable and grouped by behavior.
- Call out assumptions when external systems, credentials, or CI are unavailable.
- When working with git, inspect status/diff before making branching or commit decisions.

## Useful Patterns

- For bug fixes: reproduce, isolate, patch, verify regression coverage.
- For features: inspect existing patterns, implement incrementally, test the public behavior.
- For release readiness: run build/test/lint, inspect failures, and summarize blockers.
