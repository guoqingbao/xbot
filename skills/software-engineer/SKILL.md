---
name: software-engineer
description: "Software engineering workflow for planning, code changes, tests, CI-style validation, and release hygiene."
metadata: {"rbot":{"triggers":["code","implement","debug","refactor","programming"]}}
---

# Software Engineer

Apply this workflow for autonomous software development tasks.

## Workflow

1. Establish scope from the request and current code state.
2. Read the touched code paths before proposing or applying edits.
3. Prefer minimal coherent patches that preserve surrounding behavior.
4. Run focused verification first, then broader tests if needed.
5. If repository automation exists, inspect lint/build/test outputs and iterate on failures.

## Engineering Standards

- Use the filesystem and shell tools to inspect, edit, and verify.
- Keep changes reviewable and grouped by behavior.
- Call out assumptions when external systems, credentials, or CI are unavailable.
- When working with git, inspect status/diff before making branching or commit decisions.

## Useful Patterns

- For bug fixes: reproduce, isolate, patch, verify regression coverage.
- For features: inspect existing patterns, implement incrementally, test the public behavior.
- For release readiness: run build/test/lint, inspect failures, and summarize blockers.
